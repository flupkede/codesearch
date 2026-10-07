//! Import/dependent/similar internals — dispatch targets of the `find` and
//! `explore` tools, not tools of their own (no router). Extracted from
//! `mod.rs` (todo #105).

use super::*;
use rmcp::model::{CallToolResult, ContentBlock};

impl CodesearchService {
    pub(crate) fn normalize_symbol_query_path(&self, project_root: &Path, file: &Path) -> PathBuf {
        if file.is_absolute() {
            if let Ok(relative) = file.strip_prefix(project_root) {
                return PathBuf::from(relative.to_string_lossy().replace('\\', "/"));
            }
        }

        PathBuf::from(file.to_string_lossy().replace('\\', "/"))
    }

    pub(crate) async fn find_imports(
        &self,
        Parameters(request): Parameters<FindImportsRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "find")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![ContentBlock::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![ContentBlock::text(e)]));
            }
        }

        // In serve mode, use the resolved project root from alias_roots
        let project_root = if let Some(ref alias) = ctx.project_alias {
            ctx.alias_roots
                .get(alias)
                .map(PathBuf::from)
                .unwrap_or_else(|| self.project_path.clone())
        } else {
            self.project_path.clone()
        };
        // Strip project-alias prefix from target path if present.
        let stripped_path = strip_alias_prefix(&request.path, ctx.project_alias.as_ref());
        let normalized = normalize_tool_path(&stripped_path, &project_root);

        // Stores that failed during this lookup, so "no imports found" is never
        // reported as fact when a store never answered.
        let mut import_warnings: Vec<String> = Vec::new();

        let mut items = if let Some(ref sv) = ctx.stores_vec {
            // Multi-store group fan-out: collect import items from all stores.
            // Dedup keys on (store, id): chunk ids are per-repo counters and
            // collide across repos, so a bare-id set would drop a later repo's
            // imports wholesale.
            let import_aliases = ctx.aliases();
            let mut all_items: Vec<ImportItem> = Vec::new();
            let mut seen_ids: std::collections::HashSet<(usize, u32)> =
                std::collections::HashSet::new();
            for (store_idx, store_arc) in sv.iter().enumerate() {
                let Some(store) = try_vector_read_or_note(
                    &store_arc.vector_store,
                    import_aliases,
                    store_idx,
                    &mut import_warnings,
                    "chunk lookup",
                ) else {
                    continue;
                };
                match store.chunks_for_file(&normalized) {
                    Ok(metas) => {
                        for meta in metas {
                            if !is_import_kind(&meta.kind) {
                                continue;
                            }
                            if seen_ids.insert((store_idx, meta.id)) {
                                match store.get_chunk(meta.id) {
                                    Ok(Some(chunk)) => all_items.extend(parse_import_lines(
                                        &chunk.content,
                                        chunk.start_line,
                                    )),
                                    Ok(None) => {}
                                    Err(ref e) => note_store_failure(
                                        &mut import_warnings,
                                        import_aliases,
                                        store_idx,
                                        "chunk lookup",
                                        e,
                                    ),
                                }
                            }
                        }
                    }
                    Err(ref e) => {
                        note_store_failure(
                            &mut import_warnings,
                            import_aliases,
                            store_idx,
                            "imports scan",
                            e,
                        );
                    }
                }
            }
            all_items
        } else {
            match self
                .with_vector_store_read_for(
                    |store| {
                        let mut out = Vec::new();
                        for meta in store.chunks_for_file(&normalized)? {
                            if !is_import_kind(&meta.kind) {
                                continue;
                            }
                            if let Some(chunk) = store.get_chunk(meta.id)? {
                                out.extend(parse_import_lines(&chunk.content, chunk.start_line));
                            }
                        }
                        Ok(out)
                    },
                    ctx.stores.clone(),
                )
                .await
            {
                Ok(items) => items,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                        "Error reading imports: {e:#}"
                    ))]));
                }
            }
        };

        if items.is_empty() {
            // Fallback: no import-kind chunks found for this file. Broaden the
            // search to common import keywords and filter to the target path.
            // Limitation: this only finds chunks containing these literal words;
            // language-specific import forms that lack these keywords will be missed.
            let fallback_limit = 40usize;
            // Origin-tagged FTS hits (dedup key includes the repo: ids collide
            // across stores).
            let mut all_hits: Vec<SourcedResult<crate::fts::FtsResult>> = Vec::new();
            let mut seen_fts_ids: std::collections::HashSet<(String, u32)> =
                std::collections::HashSet::new();

            if let Some(ref sv) = ctx.stores_vec {
                let import_aliases = ctx.aliases();
                // Multi-store FTS fallback
                for keyword in IMPORT_FTS_KEYWORDS {
                    let hits = self
                        .with_fts_store_read_multi(
                            |fts_store| fts_store.search_exact(keyword, fallback_limit, None),
                            sv.clone(),
                            ctx.store_aliases.as_ref().unwrap(),
                        )
                        .await
                        .unwrap_or_default()
                        .into_results(&mut import_warnings, "imports search");
                    for h in hits {
                        if seen_fts_ids.insert((h.alias.clone(), h.result.chunk_id)) {
                            all_hits.push(h);
                        }
                    }
                }

                // Resolve each FTS hit in its ORIGIN store — probing every
                // store for a bare chunk id answers from the wrong repo on the
                // cross-repo id collisions.
                let mut resolved: Vec<ImportItem> = Vec::new();
                for hit in &all_hits {
                    let Some(origin_idx) =
                        import_aliases.iter().position(|a| *a == hit.alias)
                    else {
                        continue;
                    };
                    let store_arc = &sv[origin_idx];
                    let Some(store) = try_vector_read_or_note(
                        &store_arc.vector_store,
                        import_aliases,
                        origin_idx,
                        &mut import_warnings,
                        "chunk lookup",
                    ) else {
                        continue;
                    };
                    match store.get_chunk(hit.result.chunk_id) {
                        Ok(Some(chunk)) => {
                            if crate::cache::normalize_path_str(&chunk.path) == normalized {
                                resolved.extend(parse_import_lines(
                                    &chunk.content,
                                    chunk.start_line,
                                ));
                            }
                        }
                        Ok(None) => continue,
                        Err(ref e) => {
                            note_store_failure(
                                &mut import_warnings,
                                import_aliases,
                                origin_idx,
                                "chunk lookup",
                                e,
                            );
                            continue;
                        }
                    }
                }
                items = resolved;
            } else {
                // Single-store FTS fallback
                for keyword in IMPORT_FTS_KEYWORDS {
                    let hits = match self
                        .with_fts_store_read_for(
                            |fts_store| fts_store.search_exact(keyword, fallback_limit, None),
                            ctx.stores.clone(),
                        )
                        .await
                    {
                        Ok(h) => h,
                        Err(e) => {
                            push_store_warning(
                                &mut import_warnings,
                                &store_warning(
                                    ctx.project_alias.as_deref().unwrap_or("unknown"),
                                    "imports search",
                                    &format!("{e:#}"),
                                ),
                            );
                            Vec::new()
                        }
                    };
                    // Single store: tag with the routed alias for uniformity;
                    // resolution below runs in this same store.
                    let own_alias = ctx.project_alias.clone().unwrap_or_default();
                    for h in hits {
                        if seen_fts_ids.insert((own_alias.clone(), h.chunk_id)) {
                            all_hits.push(SourcedResult::new(own_alias.clone(), h));
                        }
                    }
                }

                items = self
                    .with_vector_store_read_for(
                        |store| {
                            let mut out = Vec::new();
                            for hit in &all_hits {
                                if let Some(chunk) = store.get_chunk(hit.result.chunk_id)? {
                                    if crate::cache::normalize_path_str(&chunk.path) == normalized {
                                        out.extend(parse_import_lines(
                                            &chunk.content,
                                            chunk.start_line,
                                        ));
                                    }
                                }
                            }
                            Ok(out)
                        },
                        ctx.stores.clone(),
                    )
                    .await
                    .unwrap_or_else(|e| {
                        push_store_warning(
                            &mut import_warnings,
                            &store_warning(
                                ctx.project_alias.as_deref().unwrap_or("unknown"),
                                "chunk lookup",
                                &format!("{e:#}"),
                            ),
                        );
                        Vec::new()
                    });
            }
        }

        items.sort_by_key(|i| i.line);
        respond_with_items(&items, &import_warnings, || {
            "No import chunks found. The index may not include import statements \
             for this language, or the file has no imports."
                .to_string()
        })
    }

    pub(crate) async fn find_dependents(
        &self,
        Parameters(request): Parameters<FindDependentsRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "find")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![ContentBlock::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![ContentBlock::text(e)]));
            }
        }

        let limit = request.limit.unwrap_or(20).min(200);
        let high_limit = (limit * 10).max(200); // generous budget for filtering

        // Stores that failed during this lookup, so "no dependents" is never
        // reported as fact when a store never answered.
        let mut dep_warnings: Vec<String> = Vec::new();

        // Extract a meaningful search term from path-like inputs.
        // Import chunks contain module references like `use crate::constants::X`
        // but the tool receives file paths like `src/constants.rs`.
        // We extract the file stem to match against module names in imports.
        let search_term = if request.symbol_or_path.contains('/')
            || request.symbol_or_path.contains('\\')
            || request.symbol_or_path.contains('.')
        {
            std::path::Path::new(&request.symbol_or_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&request.symbol_or_path)
                .to_string()
        } else {
            request.symbol_or_path.clone()
        };

        let import_kind = Some(crate::chunker::ChunkKind::Imports);

        // Two-phase search strategy:
        // 1. `search_exact` — precise term match on signature+content with
        //    MUST filter for Import kind. Strictly limits results to import chunks.
        // 2. If that yields no import-kind results, fall back to `search`
        //    (QueryParser, broader tokenization) with kind boost for imports.
        //
        // Limitation: the chunker does not emit per-statement AST import chunks;
        // imports are gap-classified as `Imports` kind. Chunks whose kind doesn't
        // match `is_import_kind()` will be missed regardless of search method.
        let fts_results = if let Some(ref sv) = ctx.stores_vec {
            let sa = ctx.store_aliases.as_ref().unwrap();
            // Multi-store FTS search
            let exact_hits = self
                .with_fts_store_read_multi(
                    |fts_store| fts_store.search_exact(&search_term, high_limit, import_kind),
                    sv.clone(),
                    sa,
                )
                .await
                .unwrap_or_default()
                .into_results(&mut dep_warnings, "dependents search");

            if exact_hits.is_empty() {
                self.with_fts_store_read_multi(
                    |fts_store| fts_store.search(&search_term, high_limit, import_kind),
                    sv.clone(),
                    sa,
                )
                .await
                .unwrap_or_default()
                .into_results(&mut dep_warnings, "dependents search")
            } else {
                exact_hits
            }
        } else {
            // Single-store FTS search — tagged for uniformity with the
            // resolution block below.
            let mut run = |r: anyhow::Result<Vec<crate::fts::FtsResult>>| match r {
                Ok(hits) => {
                    let alias = ctx.project_alias.clone().unwrap_or_else(|| "local".to_string());
                    hits.into_iter()
                        .map(|hit| SourcedResult::new(alias.clone(), hit))
                        .collect()
                }
                Err(e) => {
                    let alias = ctx.project_alias.as_deref().unwrap_or("unknown");
                    push_store_warning(
                        &mut dep_warnings,
                        &store_warning(alias, "dependents search", &format!("{e:#}")),
                    );
                    Vec::new()
                }
            };
            let exact_hits = run(self
                .with_fts_store_read_for(
                    |fts_store| fts_store.search_exact(&search_term, high_limit, import_kind),
                    ctx.stores.clone(),
                )
                .await);

            if exact_hits.is_empty() {
                run(self
                    .with_fts_store_read_for(
                        |fts_store| fts_store.search(&search_term, high_limit, import_kind),
                        ctx.stores.clone(),
                    )
                    .await)
            } else {
                exact_hits
            }
        };

        let mut items = if let Some(ref sv) = ctx.stores_vec {
            // Multi-store: resolve each hit in its ORIGIN store only — a bare
            // chunk id collides across repos and "first store that answers"
            // steals the hit (see SourcedResult).
            let dep_aliases = ctx.aliases();
            let mut seen_paths = HashSet::new();
            let mut out = Vec::new();
            let term_lower = search_term.to_lowercase();
            for f in &fts_results {
                let Some(origin_idx) = dep_aliases.iter().position(|a| *a == f.alias) else {
                    continue;
                };
                let store_arc = &sv[origin_idx];
                let Some(store) = try_vector_read_or_note(
                    &store_arc.vector_store,
                    dep_aliases,
                    origin_idx,
                    &mut dep_warnings,
                    "chunk lookup",
                ) else {
                    continue;
                };
                match store.get_chunk(f.result.chunk_id) {
                    Ok(Some(chunk)) => {
                        if !is_import_kind(&chunk.kind) {
                            continue; // try next FTS result
                        }

                        let norm = crate::cache::normalize_path_str(&chunk.path);
                        if !seen_paths.insert(norm) {
                            continue;
                        }

                        // Extract the specific import line(s) that mention the
                        // module name, rather than returning the entire chunk content.
                        let import_statement =
                            if chunk.content.to_lowercase().contains(&term_lower) {
                                chunk
                                    .content
                                    .lines()
                                    .find(|l| l.to_lowercase().contains(&term_lower))
                                    .unwrap_or("")
                                    .to_string()
                            } else {
                                chunk.signature.filter(|s| !s.is_empty()).unwrap_or(
                                    chunk.content.lines().next().unwrap_or("").to_string(),
                                )
                            };

                        out.push(DependentItem {
                            path: ctx.prefix_sourced_path(&f.alias, &chunk.path),
                            line: chunk.start_line + 1,
                            import_statement,
                        });
                    }
                    Ok(None) => {} // not held anywhere — skip
                    Err(ref e) => {
                        note_store_failure(
                            &mut dep_warnings,
                            dep_aliases,
                            origin_idx,
                            "chunk lookup",
                            e,
                        );
                    }
                }
                if out.len() >= limit {
                    break;
                }
            }
            out
        } else {
            match self
                .with_vector_store_read_for(
                    |store| {
                        let mut seen_paths = HashSet::new();
                        let mut out = Vec::new();
                        let term_lower = search_term.to_lowercase();
                        for f in &fts_results {
                            if let Some(chunk) = store.get_chunk(f.result.chunk_id)? {
                                if !is_import_kind(&chunk.kind) {
                                    continue;
                                }

                                let norm = crate::cache::normalize_path_str(&chunk.path);
                                if !seen_paths.insert(norm) {
                                    continue;
                                }

                                // Extract the specific import line(s) that mention the
                                // module name, rather than returning the entire chunk content.
                                let import_statement =
                                    if chunk.content.to_lowercase().contains(&term_lower) {
                                        chunk
                                            .content
                                            .lines()
                                            .find(|l| l.to_lowercase().contains(&term_lower))
                                            .unwrap_or("")
                                            .to_string()
                                    } else {
                                        chunk.signature.filter(|s| !s.is_empty()).unwrap_or(
                                            chunk.content.lines().next().unwrap_or("").to_string(),
                                        )
                                    };

                                out.push(DependentItem {
                                    path: chunk.path,
                                    line: chunk.start_line + 1,
                                    import_statement,
                                });

                                if out.len() >= limit {
                                    break;
                                }
                            }
                        }
                        Ok(out)
                    },
                    ctx.stores.clone(),
                )
                .await
            {
                Ok(items) => items,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                        "Error resolving dependents: {e:#}"
                    ))]));
                }
            }
        };

        // Prefix paths with the routing alias; group items were already
        // attributed to their origin repo at resolution time.
        if ctx.stores_vec.is_none() {
            for item in &mut items {
                item.path = ctx.prefix_result_path(&item.path);
            }
        }

        items.sort_by(|a, b| a.path.cmp(&b.path));
        respond_with_items(&items, &dep_warnings, || {
            format!("No dependent files found for '{}'.", request.symbol_or_path)
        })
    }

    /// Internal: find similar chunks, used by `explore(kind="similar")`.
    pub(crate) async fn similar_chunks(
        &self,
        Parameters(request): Parameters<SimilarChunksRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "explore")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![ContentBlock::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![ContentBlock::text(e)]));
            }
        }

        let limit = request.limit.unwrap_or(5).min(20);

        // Stores that failed while resolving the source embedding. `if let
        // Ok(Some(..))` used to discard the error, so a dead store produced
        // "embedding not found" — a wrong diagnosis, not a missing chunk.
        let mut similar_warnings: Vec<String> = Vec::new();

        let mut results = if let Some(ref sv) = ctx.stores_vec {
            // Multi-store: chunk ids are per-repo counters, so the source
            // embedding cannot be resolved by "first store that answers this
            // id" — that silently picks the WRONG repo's chunk on collision.
            // Same contract as get_chunk: exactly one holder auto-routes;
            // several holders are an ambiguity the caller must resolve with
            // `project=`.
            let aliases = ctx.aliases();
            let mut candidates: Vec<(usize, Vec<f32>)> = Vec::new();
            for (i, store_arc) in sv.iter().enumerate() {
                let Some(store) = try_vector_read_or_note(
                    &store_arc.vector_store,
                    aliases,
                    i,
                    &mut similar_warnings,
                    "embedding lookup",
                ) else {
                    continue;
                };
                match store.get_embedding(request.chunk_id) {
                    Ok(Some(emb)) => candidates.push((i, emb)),
                    Ok(None) => continue,
                    Err(ref e) => {
                        note_store_failure(
                            &mut similar_warnings,
                            aliases,
                            i,
                            "embedding lookup",
                            e,
                        );
                        continue;
                    }
                }
            }

            let embedding = match candidates.len() {
                0 => {
                    return Ok(CallToolResult::success(vec![ContentBlock::text(
                        qualify_empty_result(
                            format!(
                                "Embedding not found for chunk_id {} in any store.",
                                request.chunk_id
                            ),
                            &similar_warnings,
                        ),
                    )]));
                }
                1 => candidates.pop().expect("len == 1 checked").1,
                _ => {
                    let holder_names: Vec<&str> = candidates
                        .iter()
                        .map(|(i, _)| {
                            aliases
                                .get(*i)
                                .map(String::as_str)
                                .unwrap_or("unnamed store")
                        })
                        .collect();
                    return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                        "Ambiguous chunk_id {}: found in {} repositories ({}). \
                         Pass `project=` to pick one.",
                        request.chunk_id,
                        holder_names.len(),
                        holder_names.join(", ")
                    ))]));
                }
            };

            // Search across all stores with the found embedding. Dedup keys on
            // (store, id): a bare id collides across repos and would drop a
            // whole repo's neighbours whenever another repo reused the id.
            let mut all_results: Vec<SearchResultItem> = Vec::new();
            let mut seen_ids: HashSet<(usize, u32)> =
                HashSet::new();
            for (store_idx, store_arc) in sv.iter().enumerate() {
                let alias = aliases
                    .get(store_idx)
                    .map(String::as_str)
                    .unwrap_or_default();
                let Some(store) = try_vector_read_or_note(
                    &store_arc.vector_store,
                    aliases,
                    store_idx,
                    &mut similar_warnings,
                    "search",
                ) else {
                    continue;
                };
                match store.search(&embedding, limit + 1) {
                    Ok(mut neighbors) => {
                        neighbors.retain(|r| r.id != request.chunk_id);
                        for r in neighbors {
                            if seen_ids.insert((store_idx, r.id)) {
                                all_results.push(SearchResultItem {
                                    chunk_id: Some(r.id),
                                    path: ctx.prefix_sourced_path(alias, &r.path),
                                    start_line: r.start_line + 1,
                                    end_line: r.end_line + 1,
                                    kind: r.kind,
                                    score: r.score,
                                    signature: r.signature,
                                    content: None,
                                    context_prev: None,
                                    context_next: None,
                                    source: None,
                                    chunk_ref: None,
                                });
                            }
                        }
                    }
                    Err(ref e) => {
                        // The embedding was found, so the handler returns results
                        // either way; without this, a group query silently omits
                        // every neighbour from the broken repo.
                        note_store_failure(
                            &mut similar_warnings,
                            aliases,
                            store_idx,
                            "similarity search",
                            e,
                        );
                    }
                }
            }

            all_results.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            all_results.truncate(limit);
            all_results
        } else {
            match self
                .with_vector_store_read_for(
                    |store| {
                        let embedding =
                            store.get_embedding(request.chunk_id)?.ok_or_else(|| {
                                anyhow::anyhow!(
                                    "embedding not found for chunk_id {}",
                                    request.chunk_id
                                )
                            })?;

                        let mut neighbors = store.search(&embedding, limit + 1)?;
                        neighbors.retain(|r| r.id != request.chunk_id);
                        neighbors.truncate(limit);

                        let items = neighbors
                            .into_iter()
                            .map(|r| SearchResultItem {
                                chunk_id: Some(r.id),
                                path: r.path,
                                start_line: r.start_line + 1,
                                end_line: r.end_line + 1,
                                kind: r.kind,
                                score: r.score,
                                signature: r.signature,
                                content: None,
                                context_prev: None,
                                context_next: None,
                                source: None,
                                chunk_ref: None,
                            })
                            .collect::<Vec<_>>();
                        Ok(items)
                    },
                    ctx.stores.clone(),
                )
                .await
            {
                Ok(items) => items,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                        "Error finding similar chunks: {e:#}"
                    ))]));
                }
            }
        };

        // Prefix paths with the routing alias; group items were already
        // attributed to their origin repo at resolution time.
        if ctx.stores_vec.is_none() {
            for item in &mut results {
                item.path = ctx.prefix_result_path(&item.path);
            }
        }

        // Every exit carries the channel: the earlier read sat in an
        // early-return arm, so once an embedding was found, every failure
        // recorded afterwards (the whole neighbour fan-out) was discarded.
        respond_with_items(&results, &similar_warnings, || {
            format!("No similar chunks found for chunk_id {}.", request.chunk_id)
        })
    }
}
