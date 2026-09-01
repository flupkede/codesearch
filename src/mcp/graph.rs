//! Import/dependent/similar internals — dispatch targets of the `find` and
//! `explore` tools, not tools of their own (no router). Extracted from
//! `mod.rs` (todo #105).

use super::*;
use rmcp::model::{CallToolResult, Content};

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
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
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
            // Multi-store group fan-out: collect import items from all stores
            let import_aliases = ctx.aliases();
            let mut all_items: Vec<ImportItem> = Vec::new();
            let mut seen_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
            for (store_idx, store_arc) in sv.iter().enumerate() {
                let store = store_arc.vector_store.read().await;
                match store.chunks_for_file(&normalized) {
                    Ok(metas) => {
                        for meta in metas {
                            if !is_import_kind(&meta.kind) {
                                continue;
                            }
                            if seen_ids.insert(meta.id) {
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
                    return Ok(CallToolResult::success(vec![Content::text(format!(
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
            let mut all_hits: Vec<(u32, f32)> = Vec::new();
            let mut seen_fts_ids: HashSet<u32> = HashSet::new();

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
                        if seen_fts_ids.insert(h.chunk_id) {
                            all_hits.push((h.chunk_id, h.score));
                        }
                    }
                }

                // Resolve FTS hits via vector stores
                let mut resolved: Vec<ImportItem> = Vec::new();
                for (chunk_id, _) in &all_hits {
                    for (store_idx, store_arc) in sv.iter().enumerate() {
                        let store = store_arc.vector_store.read().await;
                        match store.get_chunk(*chunk_id) {
                            Ok(Some(chunk)) => {
                                if crate::cache::normalize_path_str(&chunk.path) == normalized {
                                    resolved.extend(parse_import_lines(
                                        &chunk.content,
                                        chunk.start_line,
                                    ));
                                }
                                break;
                            }
                            Ok(None) => continue,
                            Err(ref e) => {
                                note_store_failure(
                                    &mut import_warnings,
                                    import_aliases,
                                    store_idx,
                                    "chunk lookup",
                                    e,
                                );
                                continue;
                            }
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
                    for h in hits {
                        if seen_fts_ids.insert(h.chunk_id) {
                            all_hits.push((h.chunk_id, h.score));
                        }
                    }
                }

                items = self
                    .with_vector_store_read_for(
                        |store| {
                            let mut out = Vec::new();
                            for (chunk_id, _) in &all_hits {
                                if let Some(chunk) = store.get_chunk(*chunk_id)? {
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
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
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
            // Single-store FTS search
            let alias = ctx.project_alias.as_deref().unwrap_or("unknown");
            let mut run = |r: anyhow::Result<Vec<crate::fts::FtsResult>>| match r {
                Ok(hits) => hits,
                Err(e) => {
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
            // Multi-store: resolve chunks across all stores
            let dep_aliases = ctx.aliases();
            let mut seen_paths = HashSet::new();
            let mut out = Vec::new();
            for f in &fts_results {
                for (store_idx, store_arc) in sv.iter().enumerate() {
                    let store = store_arc.vector_store.read().await;
                    match store.get_chunk(f.chunk_id) {
                        Ok(Some(chunk)) => {
                            if !is_import_kind(&chunk.kind) {
                                break; // try next FTS result
                            }

                            let norm = crate::cache::normalize_path_str(&chunk.path);
                            if !seen_paths.insert(norm) {
                                break;
                            }

                            let term_lower = search_term.to_lowercase();
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
                                line: chunk.start_line,
                                import_statement,
                            });

                            break; // found in this store, move to next FTS result
                        }
                        Ok(None) => {} // try next store
                        // One broken store says nothing about the others; a
                        // `break` here silently drops a chunk that lives in a
                        // healthy store later in the list.
                        Err(ref e) => {
                            note_store_failure(
                                &mut dep_warnings,
                                dep_aliases,
                                store_idx,
                                "chunk lookup",
                                e,
                            );
                            continue;
                        }
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
                            if let Some(chunk) = store.get_chunk(f.chunk_id)? {
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
                                    line: chunk.start_line,
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
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error resolving dependents: {e:#}"
                    ))]));
                }
            }
        };

        // Prefix paths with alias for multi-repo identification
        for item in &mut items {
            item.path = ctx.prefix_result_path(&item.path);
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
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        let limit = request.limit.unwrap_or(5).min(20);

        // Stores that failed while resolving the source embedding. `if let
        // Ok(Some(..))` used to discard the error, so a dead store produced
        // "embedding not found" — a wrong diagnosis, not a missing chunk.
        let mut similar_warnings: Vec<String> = Vec::new();

        let mut results = if let Some(ref sv) = ctx.stores_vec {
            // Multi-store: find the embedding in whichever store has it,
            // then search across all stores for similar chunks.
            let aliases = ctx.aliases();
            let mut embedding: Option<Vec<f32>> = None;
            for (i, store_arc) in sv.iter().enumerate() {
                let store = store_arc.vector_store.read().await;
                match store.get_embedding(request.chunk_id) {
                    Ok(Some(emb)) => {
                        embedding = Some(emb);
                        break;
                    }
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

            let embedding = match embedding {
                Some(e) => e,
                None => {
                    return Ok(CallToolResult::success(vec![Content::text(
                        qualify_empty_result(
                            format!(
                                "Embedding not found for chunk_id {} in any store.",
                                request.chunk_id
                            ),
                            &similar_warnings,
                        ),
                    )]));
                }
            };

            // Search across all stores with the found embedding
            let mut all_results: Vec<SearchResultItem> = Vec::new();
            let mut seen_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
            for (store_idx, store_arc) in sv.iter().enumerate() {
                let store = store_arc.vector_store.read().await;
                match store.search(&embedding, limit + 1) {
                    Ok(mut neighbors) => {
                        neighbors.retain(|r| r.id != request.chunk_id);
                        for r in neighbors {
                            if seen_ids.insert(r.id) {
                                all_results.push(SearchResultItem {
                                    chunk_id: Some(r.id),
                                    path: r.path,
                                    start_line: r.start_line,
                                    end_line: r.end_line,
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
                                start_line: r.start_line,
                                end_line: r.end_line,
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
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error finding similar chunks: {e:#}"
                    ))]));
                }
            }
        };

        // Prefix paths with alias for multi-repo identification
        for item in &mut results {
            item.path = ctx.prefix_result_path(&item.path);
        }

        // Every exit carries the channel: the earlier read sat in an
        // early-return arm, so once an embedding was found, every failure
        // recorded afterwards (the whole neighbour fan-out) was discarded.
        respond_with_items(&results, &similar_warnings, || {
            format!("No similar chunks found for chunk_id {}.", request.chunk_id)
        })
    }
}
