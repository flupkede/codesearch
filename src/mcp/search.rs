//! Consolidated `search` tool (semantic/lexical dispatch) + the semantic
//! search machinery it drives. Extracted from `mod.rs` (todo #105) — the
//! `#[tool]` method registers through the per-module router merged in
//! `mod.rs`'s `merged_tool_router`.

use super::*;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    tool, tool_router, ErrorData as McpError,
};

#[tool_router(router = search_router, vis = "pub(crate)")]
impl CodesearchService {
    // Consolidated tools (the primary 5-tool surface)
    // ─────────────────────────────────────────────────────────────────

    /// Unified search tool — dispatches to semantic or literal search based on `mode`.
    #[tool(
        description = "Unified code search. Set `mode` to choose the backend:\n\n- `semantic` (default): vector embeddings + BM25 FTS + exact-identifier boosting, fused with RRF. Best for conceptual queries, identifier lookups, and mixed natural-language + symbol queries.\n- `literal`: pure FTS, no embeddings. Fast and works without an embedding model. Sub-mode selection:\n  * Queries with operators, brackets, or punctuation (`foo = null`, `Vec<T>`, `return x;`, `a::b`) -> set `regex=true` and write the query as a regex. BM25 tokenizes on punctuation otherwise, producing noisy results.\n  * Multi-word exact phrases -> set `phrase=true`.\n  * Plain identifier lookups (`CodesearchService`) -> leave both false.\n\nFor semantic mode, optionally set `semantic_mode`: \"auto\" (default) | \"semantic\" | \"lexical\" | \"hybrid\".\nSet `min_score` to drop weak hits and answer with an explicit refusal (empty results + note) instead of nearest-neighbour noise; the score scale depends on `semantic_mode` — cosine similarity for \"semantic\", RRF points (rarely above 0.2) otherwise. Federated group/project queries forward `min_score` to every leg and surface each leg's refusal note in `warnings`. Not supported in `literal` mode — passing `min_score` with mode=\"literal\" returns an explicit error naming the field.\nReturns metadata only by default (`compact=true`). Use `get_chunk` to read full code. Prefer `search(mode=\"literal\", regex=true)` over external grep/ripgrep for code patterns.\n\nIMPORTANT (multi-repo): always specify either `project` (single repo) or `group` (cross-repo). Omitting both in multi-repo mode returns a `scope_required` error with the list of available projects and groups. If the user has not indicated which repository to search, ask them to choose."
    )]
    pub(crate) async fn search(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        tracing::info!(
            "📥 search(query={:?}, mode={:?}, project={:?}, group={:?})",
            request.query,
            request.mode,
            request.project,
            request.group,
        );

        // `min_score` has no honest meaning in literal mode — BM25 scores are
        // corpus-relative and unbounded, so a caller-supplied threshold cannot
        // be calibrated the way semantic thresholds are. Refuse by name
        // instead of silently dropping the field: the tool advertises it, and
        // a silently ignored filter reads as "no weak hits existed". Checked
        // before any routing so local, federated-group and mounted-project
        // paths all behave identically.
        if request.min_score.is_some()
            && request
                .mode
                .as_deref()
                .is_some_and(|m| m.eq_ignore_ascii_case("literal"))
        {
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                "min_score is not supported in literal mode: BM25 scores are corpus-relative \
                 and unbounded, so a threshold would drop hits by an arbitrary rule. Re-run \
                 without min_score, or use mode=\"semantic\" — there an emptied result set \
                 comes back with an explicit refusal note naming the threshold.",
            )]));
        }

        // Federation: when the query targets a group that resolves to one or more
        // remote peers, merge local + remote results (RRF-interleave) instead of
        // searching local repos only. Only `group` federates; `project` stays
        // local because project aliases are instance-local.
        if let Some(group) = request.group.as_deref() {
            let cfg = self.federation_config();
            if Self::group_has_remotes(&cfg, group) {
                let remote_projects = cfg.group_remote_projects(group);
                return self.federated_search(&request, &cfg, remote_projects).await;
            }
        }

        // Project-level federation (mounted remote project): a `project` of the
        // form "<peer>/<alias>" transparently routes to that single peer's own
        // `<alias>` project — a 1-to-1 passthrough, as if the index were local.
        // Local repos ALWAYS win a name clash: only route remotely when the name
        // does not resolve to a local project.
        if let Some(proj) = request.project.as_deref() {
            let cfg = self.federation_config();
            if cfg.resolve(proj).is_none() {
                if let Some(crate::db_discovery::repos::Target::RemoteProject {
                    peer_name,
                    peer,
                    remote_alias,
                }) = cfg.resolve_remote_project(proj)
                {
                    return self
                        .federated_project_search(&request, peer_name, peer, remote_alias)
                        .await;
                }
            }
        }

        let mode = request.mode.as_deref().unwrap_or("semantic").to_lowercase();
        match mode.as_str() {
            "semantic" => {
                // Delegate to the existing semantic_search implementation
                let semantic_req = SemanticSearchRequest {
                    query: request.query,
                    limit: request.limit,
                    compact: request.compact,
                    filter_path: request.filter_path,
                    mode: request.semantic_mode,
                    project: request.project,
                    group: request.group,
                    min_score: request.min_score,
                };
                self.semantic_search(Parameters(semantic_req)).await
            }
            "literal" => {
                // Delegate to the existing literal_search implementation
                let literal_req = LiteralSearchRequest {
                    query: request.query,
                    regex: request.regex,
                    phrase: request.phrase,
                    limit: request.limit,
                    file_glob: request.file_glob,
                    language: request.language,
                    format: request.format,
                    project: request.project,
                    group: request.group,
                };
                self.literal_search(Parameters(literal_req)).await
            }
            _ => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "Unknown search mode '{}'. Use `semantic` or `literal`.",
                mode
            ))])),
        }
    }

    // Internal implementations (called by consolidated tools above)
    // ─────────────────────────────────────────────────────────────────

    /// Internal: semantic/hybrid search implementation used by `search(mode="semantic")`.
    pub(crate) async fn semantic_search(
        &self,
        Parameters(request): Parameters<SemanticSearchRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing (multi-store for group fan-out)
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "search")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![ContentBlock::text(e)])),
        };

        let limit = request.limit.unwrap_or(10);
        let compact = request.compact.unwrap_or(true);
        let mode = request.mode.as_deref().unwrap_or("auto");
        let identifiers = detect_identifiers(&request.query);
        let has_identifiers = !identifiers.is_empty();

        tracing::debug!(
            "MCP semantic_search: query='{}', limit={}, compact={}, mode='{}', multi={}",
            request.query,
            limit,
            compact,
            mode,
            ctx.is_multi
        );

        // Ensure database exists (skip if serve-mode with routed stores)
        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![ContentBlock::text(e)]));
            }
        }

        // === Multi-store group fan-out ===
        if ctx.is_multi {
            return self
                .semantic_search_multi(
                    &request,
                    &identifiers,
                    limit,
                    compact,
                    ctx.stores_vec.unwrap(),
                    ctx.store_aliases.as_ref().unwrap(),
                    &ctx.alias_roots,
                )
                .await;
        }

        // === Mode: "lexical" — FTS only, no embedding ===
        if mode == "lexical" {
            tracing::debug!("MCP: mode=lexical — skipping embedding service");
            return self
                .semantic_search_lexical(
                    &request,
                    &identifiers,
                    limit,
                    compact,
                    ctx.stores,
                    ctx.project_alias.as_deref(),
                    &ctx.alias_roots,
                )
                .await;
        }

        // === Modes: "semantic", "hybrid", "auto" — require embedding ===
        // The query MUST be embedded with the model the target index was built
        // with. In serve mode that is the routed repo's recorded model, not a
        // hub-wide default: a 384-dim query against a 768-dim EmbeddingGemma
        // index failed with "expected 768, got 384". A repo that records no
        // model is queried with the built-in default, and the caller is warned.
        let model_resolution = self.resolve_query_model(ctx.project_alias.as_deref());
        let query_embedding = {
            let model = model_resolution.model;
            let service = match self.embedding_service_for_query(model).await {
                Ok(Some(s)) => s,
                Ok(None) => {
                    return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                        "Embedding model '{}' is not available locally, and search never \
                         downloads models. Populate the model cache (codesearch setup) or \
                         re-index this repo, then retry. Literal search (mode=\"literal\") \
                         works without embedding models.",
                        model.short_name()
                    ))]));
                }
                Err(e) => {
                    tracing::error!("MCP: Failed to get embedding service: {:?}", e);
                    return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                        "Error initializing embedding service: {e:#}"
                    ))]));
                }
            };

            tracing::debug!(
                "MCP: Embedding query with model '{}'...",
                model.short_name()
            );
            // ONNX inference is CPU-bound; running it inline pins an async
            // worker for the whole pass. Poison is recovered rather than
            // propagated — same contract as embed_chunks_yielding.
            let query_text = request.query.clone();
            match tokio::task::spawn_blocking(move || {
                let mut guard = service
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.embed_query(&query_text)
            })
            .await
            {
                Ok(Ok(e)) => e,
                Ok(Err(e)) => {
                    tracing::error!("MCP: Failed to embed query: {:?}", e);
                    return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                        "Error embedding query: {e:#}"
                    ))]));
                }
                Err(e) => {
                    tracing::error!("MCP: Embedding task failed: {:?}", e);
                    return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                        "Error embedding query: {e}"
                    ))]));
                }
            }
        };

        // Failures on this single-store path. The group fan-out has carried a
        // warnings channel since the read-only incident; without the same thing
        // here, `project=<alias>` — the form an agent uses most — still reports
        // a broken store as an ordinary empty result.
        let mut single_warnings: Vec<String> = Vec::new();
        // Surface the assumed-model warning even when the store read succeeds:
        // mismatched vector spaces do not error, they just rank wrongly.
        if let Some(warning) = model_resolution.assumed_warning {
            single_warnings.push(warning);
        }

        // Search vector store
        let vector_results = match self
            .with_vector_store_read_for(
                |store| {
                    store
                        .search(&query_embedding, limit * 5)
                        .context("Error searching vector store")
                },
                ctx.stores.clone(),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("MCP: Search failed: {:?}", e);
                // Only "semantic" has no second backend to fall back on. In
                // hybrid/auto the FTS half can still answer, so hard-failing
                // here would throw away good results — the same mistake this
                // branch already fixed once in the group fan-out.
                //
                // `{:#}` renders the whole anyhow chain. With plain `{}` the
                // caller only ever saw the outermost `.context(...)` wrapper
                // ("Error reading from project-routed vector store"), which
                // hides the actual fault and makes remote diagnosis guesswork.
                if mode == "semantic" {
                    return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                        "Error searching vector store: {:#}",
                        e
                    ))]));
                }
                single_warnings.push(format!("vector search failed: {e:#}"));
                Vec::new()
            }
        };

        tracing::debug!("MCP: Found {} vector results", vector_results.len());

        // === Mode: "semantic" — vector only, skip FTS fusion ===
        if mode == "semantic" {
            tracing::debug!("MCP: mode=semantic — using vector results only");
            let fused = vector_only(&vector_results);

            let chunk_to_result: std::collections::HashMap<u32, &crate::vectordb::SearchResult> =
                vector_results.iter().map(|r| (r.id, r)).collect();

            let mut results: Vec<crate::vectordb::SearchResult> = Vec::new();
            for f in fused.into_iter().take(limit) {
                if let Some(result) = chunk_to_result.get(&f.chunk_id) {
                    let mut r = (*result).clone();
                    r.score = f.rrf_score;
                    results.push(r);
                }
            }
            return self.build_semantic_response(
                results,
                &request,
                compact,
                has_identifiers,
                ctx.project_alias.as_deref(),
                &ctx.alias_roots,
                &single_warnings,
            );
        }

        // === Modes: "hybrid" | "auto" — full hybrid search ===
        let structural_intent = detect_structural_intent(&request.query);
        let (vector_k, fts_k) = adapt_rrf_k(&request.query);

        tracing::debug!(
            "MCP: Query analysis - identifiers: {:?}, structural_intent: {:?}, rrf_k: ({}, {})",
            identifiers,
            structural_intent,
            vector_k,
            fts_k
        );

        // Perform FTS search and fusion
        let mut results = match self
            .with_fts_store_read_for(
                |fts_store| {
                    let fts_results = fts_store
                        .search(&request.query, limit * 5, structural_intent)
                        .context("Error searching FTS store")?;

                    let fused = if identifiers.is_empty() {
                        rrf_fusion(&vector_results, &fts_results, vector_k as f32)
                    } else {
                        let mut all_exact: Vec<crate::fts::FtsResult> = Vec::new();
                        for ident in &identifiers {
                            if let Ok(exact) =
                                fts_store.search_exact(ident, limit * 3, structural_intent)
                            {
                                for r in exact {
                                    if !all_exact.iter().any(|e| e.chunk_id == r.chunk_id) {
                                        all_exact.push(r);
                                    }
                                }
                            }
                        }

                        tracing::debug!(
                            "MCP: FTS found {} results, exact found {} results",
                            fts_results.len(),
                            all_exact.len()
                        );

                        rrf_fusion_with_exact(
                            &vector_results,
                            &fts_results,
                            &all_exact,
                            vector_k as f32,
                            fts_k as f32,
                            EXACT_MATCH_RRF_K,
                        )
                    };

                    Ok(fused)
                },
                ctx.stores.clone(),
            )
            .await
        {
            Ok(fused) => {
                // Map FusedResult back to SearchResult
                let chunk_to_result: std::collections::HashMap<
                    u32,
                    &crate::vectordb::SearchResult,
                > = vector_results.iter().map(|r| (r.id, r)).collect();

                let mut mapped: Vec<crate::vectordb::SearchResult> = Vec::new();
                for f in fused.into_iter().take(limit) {
                    if let Some(result) = chunk_to_result.get(&f.chunk_id) {
                        let mut r = (*result).clone();
                        r.score = f.rrf_score;
                        mapped.push(r);
                    }
                }
                mapped
            }
            Err(e) => {
                tracing::warn!("MCP: FTS store unavailable, using vector-only: {:?}", e);
                // Degrading to vector-only is correct, but it must be VISIBLE:
                // a caller that gets half a hybrid search with no signal cannot
                // tell it from a complete one.
                single_warnings.push(format!("lexical (FTS) search failed: {e:#}"));
                vector_results.into_iter().take(limit).collect()
            }
        };

        // Apply language boost
        if let Some((_, _, Some(primary_lang))) = crate::search::read_metadata(&self.db_path) {
            for result in &mut results {
                let file_lang = format!(
                    "{:?}",
                    Language::from_path(std::path::Path::new(&result.path))
                );
                if file_lang.to_lowercase() == primary_lang.to_lowercase() {
                    result.score *= 1.2;
                }
            }
            results.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }

        // Apply kind boost
        if let Some(target_kind) = structural_intent {
            boost_kind(&mut results, target_kind);
        }

        // Auto-fallback: if hybrid search returned very few results for a code-like query,
        // run literal FTS and merge missing chunks.
        if results.len() < 3 && has_identifiers {
            tracing::debug!(
                "Auto-fallback: semantic returned {} results, trying literal",
                results.len()
            );

            let literal_results = self
                .with_fts_store_read_for(
                    |fts_store| fts_store.search(&request.query, limit, None),
                    ctx.stores.clone(),
                )
                .await
                .unwrap_or_default();

            let mut existing_ids: std::collections::HashSet<u32> =
                results.iter().map(|r| r.id).collect();

            for fts in literal_results {
                if results.len() >= limit {
                    break;
                }
                if existing_ids.contains(&fts.chunk_id) {
                    continue;
                }

                let maybe_resolved = match self
                    .with_vector_store_read_for(
                        |store| {
                            // `Ok(None)` means "this store does not hold that
                            // chunk" — a normal miss to skip. `Err` means the
                            // store is broken and must propagate: flattening
                            // the two silently dropped every remaining literal
                            // hit whenever the vector store was down, turning a
                            // dead store into an ordinary-looking short result.
                            let chunk = match store.get_chunk(fts.chunk_id)? {
                                Some(c) => c,
                                None => return Ok(None),
                            };
                            Ok(Some(crate::vectordb::SearchResult {
                                id: fts.chunk_id,
                                content: chunk.content,
                                path: chunk.path,
                                start_line: chunk.start_line,
                                end_line: chunk.end_line,
                                kind: chunk.kind,
                                signature: chunk.signature,
                                docstring: chunk.docstring,
                                context: chunk.context,
                                hash: chunk.hash,
                                distance: 0.0,
                                score: fts.score,
                                context_prev: chunk.context_prev,
                                context_next: chunk.context_next,
                            }))
                        },
                        ctx.stores.clone(),
                    )
                    .await
                {
                    Ok(resolved) => resolved,
                    Err(e) => {
                        // The old `.ok()` here folded a dead store into "no
                        // more literal hits" with zero signal to the caller —
                        // the exact false negative `single_warnings` exists
                        // for. Note it and stop: every further lookup against
                        // this store would fail the same way.
                        single_warnings.push(format!("literal-hit chunk lookup failed: {e:#}"));
                        break;
                    }
                };

                if let Some(resolved) = maybe_resolved {
                    existing_ids.insert(resolved.id);
                    results.push(resolved);
                }
            }
        }

        tracing::debug!("MCP: Final {} results after hybrid search", results.len());
        self.build_semantic_response(
            results,
            &request,
            compact,
            has_identifiers,
            ctx.project_alias.as_deref(),
            &ctx.alias_roots,
            &single_warnings,
        )
    }

    // === Helper methods (not exposed as tools) ===

    /// Multi-store semantic search: fan out across all stores, merge raw vector/FTS
    /// results, then apply RRF fusion.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn semantic_search_multi(
        &self,
        request: &SemanticSearchRequest,
        identifiers: &[String],
        limit: usize,
        compact: bool,
        stores: Vec<Arc<SharedStores>>,
        aliases: &[String],
        alias_roots: &std::collections::HashMap<String, String>,
    ) -> Result<CallToolResult, McpError> {
        let mode = request.mode.as_deref().unwrap_or("auto");
        let structural_intent = detect_structural_intent(&request.query);

        // === Lexical mode: FTS only across all stores ===
        if mode == "lexical" {
            // Lexical has no second backend, so a failed store here is invisible
            // unless it is reported: the query simply looks like it found nothing.
            let mut lexical_warnings: Vec<String> = Vec::new();

            let outcome = self
                .with_fts_store_read_multi(
                    |fts_store| fts_store.search(&request.query, limit * 5, structural_intent),
                    stores.clone(),
                    aliases,
                )
                .await
                .unwrap_or_default();
            if !outcome.failures.is_empty() {
                tracing::error!(
                    "MCP: lexical fan-out degraded — {} of {} repo(s) failed: {:?}",
                    outcome.failures.len(),
                    stores.len(),
                    outcome.failures
                );
                lexical_warnings.extend(outcome.warnings("literal search"));
            }
            let fts_results = outcome.results;

            // Also do exact search if identifiers detected
            let mut all_fts = fts_results;
            for ident in identifiers {
                let exact_outcome = self
                    .with_fts_store_read_multi(
                        |fts_store| fts_store.search_exact(ident, limit * 3, structural_intent),
                        stores.clone(),
                        aliases,
                    )
                    .await
                    .unwrap_or_default();
                lexical_warnings.extend(exact_outcome.warnings("exact-identifier search"));
                merge_exact_into_fts_multi(&mut all_fts, exact_outcome.results);
            }

            all_fts.sort_by(|a, b| {
                b.result
                    .score
                    .partial_cmp(&a.result.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            let mut results = self
                .resolve_fts_to_search_results_multi(
                    &all_fts, limit, &stores, aliases, alias_roots, &mut lexical_warnings,
                )
                .await;

            if let Some(target_kind) = structural_intent {
                boost_kind(&mut results, target_kind);
                return self.build_semantic_response(
                    results,
                    request,
                    compact,
                    !identifiers.is_empty(),
                    None,
                    alias_roots,
                    &lexical_warnings,
                );
            }

            return self.build_semantic_response(
                results,
                request,
                compact,
                !identifiers.is_empty(),
                None,
                alias_roots,
                &lexical_warnings,
            );
        }

        // === Modes requiring embedding: "semantic", "hybrid", "auto" ===
        //
        // Each repo may have been indexed with a different model, so the query
        // is embedded once per distinct model and every store is searched with
        // the embedding of ITS model. Embedding all stores with one hub-wide
        // default is what produced "Query embedding dimension mismatch:
        // expected 768, got 384" on a mixed hub.
        let mut embeddings_by_alias: std::collections::HashMap<String, Vec<f32>> =
            std::collections::HashMap::with_capacity(aliases.len());
        // Assumed-model warnings, one per repo that records no model. Collected
        // here and folded into `search_warnings` below so an agent sees the
        // assumption alongside the results it applies to.
        let mut model_warnings: Vec<String> = Vec::new();
        // Repos whose model is unavailable to search (not loaded and not on
        // disk). Dropped from the VECTOR fan-out only — the FTS pass still
        // covers them, since literal search needs no embeddings.
        let mut no_model_aliases: Vec<String> = Vec::new();
        // Distinct models that actually answered (embedding resolved): a
        // group mixing them produces scores from different vector spaces,
        // which the merge presents as one ranking.
        let mut answering_models: std::collections::HashSet<crate::embed::ModelType> =
            std::collections::HashSet::new();
        {
            let mut by_model: std::collections::HashMap<crate::embed::ModelType, Vec<f32>> =
                std::collections::HashMap::new();
            // One warning per distinct missing model, not per repo: a group
            // often shares a single model, and N copies of the same warning is
            // noise an agent has to read past.
            let mut missing_by_model: std::collections::HashMap<
                crate::embed::ModelType,
                Vec<String>,
            > = std::collections::HashMap::new();
            for alias in aliases {
                let model_resolution = self.resolve_query_model(Some(alias));
                let model = model_resolution.model;
                if let Some(warning) = model_resolution.assumed_warning {
                    model_warnings.push(warning);
                }
                let embedding = match by_model.get(&model) {
                    Some(cached) => cached.clone(),
                    None => match self.embedding_service_for_query(model).await {
                        Ok(Some(service)) => {
                            // ONNX inference is CPU-bound; running it inline
                            // pins an async worker for the whole pass. Poison
                            // recovery — same contract as the single-project
                            // path.
                            let query_text = request.query.clone();
                            let embedding = match tokio::task::spawn_blocking(move || {
                                let mut guard = service
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                guard.embed_query(&query_text)
                            })
                            .await
                            {
                                Ok(Ok(e)) => e,
                                Ok(Err(e)) => {
                                    return Ok(CallToolResult::success(vec![
                                        ContentBlock::text(format!(
                                            "Error embedding query: {e:#}"
                                        )),
                                    ]));
                                }
                                Err(e) => {
                                    return Ok(CallToolResult::success(vec![
                                        ContentBlock::text(format!(
                                            "Embedding task failed: {e}"
                                        )),
                                    ]));
                                }
                            };
                            by_model.insert(model, embedding.clone());
                            embedding
                        }
                        Ok(None) => {
                            missing_by_model
                                .entry(model)
                                .or_default()
                                .push(alias.clone());
                            continue;
                        }
                        Err(e) => {
                            return Ok(CallToolResult::success(vec![ContentBlock::text(
                                format!(
                                    "Error initializing embedding service for '{alias}': {e:#}"
                                ),
                            )]));
                        }
                    },
                };
                embeddings_by_alias.insert(alias.clone(), embedding);
                answering_models.insert(model);
            }
            for (model, missing) in missing_by_model {
                model_warnings.push(format!(
                    "semantic skipped for {} repo(s) using model '{}' not available \
                     locally (search never downloads models; populate the cache via \
                     codesearch setup or re-index): {}",
                    missing.len(),
                    model.short_name(),
                    missing.join(", ")
                ));
                no_model_aliases.extend(missing);
            }
        }

        // A mixed-model group answers with scores from different vector
        // spaces, which the merged ranking presents as one scale. Warn in
        // every mode: RRF fusion (auto/hybrid) partially hides it by fusing
        // ranks only; semantic_mode's raw cosine scores expose it directly.
        if let Some(warning) = mixed_model_group_warning(answering_models) {
            model_warnings.push(warning);
        }

        // Search vector stores across all repos, each with its own model's
        // query embedding. Results stay origin-tagged (see SourcedResult) —
        // the tag replaces the old alias_by_chunk side-map, which keyed on
        // bare chunk ids and could not survive the cross-repo collisions.
        //
        // Only repos that got a query embedding take part: a repo whose model
        // is unavailable locally must not force a download — or, on a
        // black-holed network, an indefinite wedge — for the whole group.
        // The FTS pass below still covers skipped repos in hybrid/auto/
        // lexical, so they degrade, not disappear.
        let skip_set: std::collections::HashSet<&str> =
            no_model_aliases.iter().map(String::as_str).collect();
        let vector_stores: Vec<Arc<SharedStores>> = stores
            .iter()
            .zip(aliases.iter())
            .filter(|(_, alias)| !skip_set.contains(alias.as_str()))
            .map(|(store, _)| Arc::clone(store))
            .collect();
        let vector_aliases: Vec<String> = aliases
            .iter()
            .filter(|alias| !skip_set.contains(alias.as_str()))
            .cloned()
            .collect();
        if mode == "semantic" && !no_model_aliases.is_empty() && vector_stores.is_empty() {
            let detail = model_warnings.join("\n");
            return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "Semantic search could not run: every repo in this group was skipped \
                 because its embedding model is missing locally.\n{detail}\nLiteral \
                 search (mode=\"literal\") works without embedding models."
            ))]));
        }
        let outcome = self
            .with_vector_store_read_multi(
                |alias, store| {
                    let embedding = embeddings_by_alias.get(alias).ok_or_else(|| {
                        anyhow::anyhow!(
                            "internal error: no query embedding resolved for repo '{alias}'"
                        )
                    })?;
                    let found = store
                        .search(embedding, limit * 5)
                        .context("Error searching vector store")?;
                    Ok(found)
                },
                vector_stores,
                &vector_aliases,
            )
            .await;

        // Warnings raised by the fan-out, carried into the response so the
        // calling agent can tell "not in the corpus" from "that repo is down".
        // Seeded with any assumed-model warnings gathered while embedding.
        let mut search_warnings: Vec<String> = model_warnings;

        let vector_results =
            match outcome {
                Ok(o) => {
                    if !o.failures.is_empty() {
                        tracing::error!(
                            "MCP: vector fan-out degraded — {} of {} repo(s) failed: {:?}",
                            o.failures.len(),
                            vector_aliases.len(),
                            o.failures
                        );
                        // Only "semantic" has no second backend to fall back on. In
                        // hybrid/auto/lexical the FTS half can still answer, so
                        // hard-failing here would throw away good results — the same
                        // reason one broken repo does not abort the whole fan-out.
                        if mode == "semantic" && o.results.is_empty() {
                            let detail = o
                                .failures
                                .iter()
                                .map(|(alias, err)| format!("  - {alias}: {err}"))
                                .collect::<Vec<_>>()
                                .join("\n");
                            return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                                "Error searching vector store: {} of {} repo(s) in scope failed \
                             and none returned results:\n{}",
                                o.failures.len(),
                                vector_aliases.len(),
                                detail
                            ))]));
                        }
                        search_warnings.extend(o.failures.iter().map(|(alias, err)| {
                            format!("repo '{alias}' vector search failed: {err}")
                        }));
                    }
                    o.results
                }
                Err(e) => {
                    tracing::error!("MCP: vector fan-out failed: {:?}", e);
                    return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                        "Error searching vector store: {e:#}"
                    ))]));
                }
            };

        // === Mode: "semantic" — vector only ===
        if mode == "semantic" {
            // Vector-only fan-out: no second backend to fuse with, so the RRF
            // pass would be an identity function. Iterate the origin-tagged
            // hits directly, prefixing each path from its own tag — the old
            // `vector_only` + id-keyed map folded different repos' chunks
            // onto one entry whenever their local ids collided.
            let mut results: Vec<crate::vectordb::SearchResult> = Vec::new();
            for hit in vector_results.iter().take(limit) {
                let mut r = hit.result.clone();
                r.path = prefix_path_with_alias(
                    &r.path,
                    Some(&hit.alias),
                    alias_roots
                        .get(&hit.alias)
                        .map(String::as_str)
                        .unwrap_or(""),
                );
                results.push(r);
            }
            return self.build_semantic_response(
                results,
                request,
                compact,
                !identifiers.is_empty(),
                None,
                alias_roots,
                &search_warnings,
            );
        }

        // === Modes: "hybrid" | "auto" — full hybrid search ===
        let (vector_k, fts_k) = adapt_rrf_k(&request.query);

        // FTS search across all stores. Its failures matter as much as the
        // vector half's: during the cloud read-only incident literal search
        // also returned 0 results for every affected vendor, and looked clean.
        let fts_outcome = self
            .with_fts_store_read_multi(
                |fts_store| fts_store.search(&request.query, limit * 5, structural_intent),
                stores.clone(),
                aliases,
            )
            .await
            .unwrap_or_default();
        if !fts_outcome.failures.is_empty() {
            tracing::error!(
                "MCP: FTS fan-out degraded — {} of {} repo(s) failed: {:?}",
                fts_outcome.failures.len(),
                stores.len(),
                fts_outcome.failures
            );
            search_warnings.extend(fts_outcome.warnings("literal search"));
        }
        let fts_results = fts_outcome.results;

        // Exact identifier search across all stores
        let all_exact = if !identifiers.is_empty() {
            let mut exact_results: Vec<SourcedResult<crate::fts::FtsResult>> = Vec::new();
            for ident in identifiers {
                let exact_outcome = self
                    .with_fts_store_read_multi(
                        |fts_store| fts_store.search_exact(ident, limit * 3, structural_intent),
                        stores.clone(),
                        aliases,
                    )
                    .await
                    .unwrap_or_default();
                search_warnings.extend(exact_outcome.warnings("exact-identifier search"));
                for r in exact_outcome.results {
                    let key = (r.alias.clone(), r.result.chunk_id);
                    if !exact_results
                        .iter()
                        .any(|e| (e.alias.clone(), e.result.chunk_id) == key)
                    {
                        exact_results.push(r);
                    }
                }
            }
            exact_results
        } else {
            Vec::new()
        };

        // The pure fusion functions key on a single bare u32 — but chunk ids
        // are per-repo counters and collide across a group, which fused
        // different repos' chunks into one entry. Intern every (repo, chunk)
        // pair to a unique synthetic id for the fusion call; the fused ids
        // map straight back to their owning repo below.
        let mut interned: std::collections::HashMap<(String, u32), u32> =
            std::collections::HashMap::new();

        let vector_translated: Vec<crate::vectordb::SearchResult> = vector_results
            .iter()
            .map(|hit| {
                let mut r = hit.result.clone();
                r.id =
                    intern_group_chunk_id(&mut interned, (hit.alias.clone(), r.id));
                r
            })
            .collect();
        let fts_translated: Vec<crate::fts::FtsResult> = fts_results
            .iter()
            .map(|hit| crate::fts::FtsResult {
                chunk_id: intern_group_chunk_id(
                    &mut interned,
                    (hit.alias.clone(), hit.result.chunk_id),
                ),
                score: hit.result.score,
            })
            .collect();
        let exact_translated: Vec<crate::fts::FtsResult> = all_exact
            .iter()
            .map(|hit| crate::fts::FtsResult {
                chunk_id: intern_group_chunk_id(
                    &mut interned,
                    (hit.alias.clone(), hit.result.chunk_id),
                ),
                score: hit.result.score,
            })
            .collect();

        // RRF fusion
        let fused = if identifiers.is_empty() {
            rrf_fusion(&vector_translated, &fts_translated, vector_k as f32)
        } else {
            rrf_fusion_with_exact(
                &vector_translated,
                &fts_translated,
                &exact_translated,
                vector_k as f32,
                fts_k as f32,
                EXACT_MATCH_RRF_K,
            )
        };

        // Reverse side of the interning table: synthetic id -> (repo, chunk id).
        let origins: Vec<(String, u32)> = {
            let mut v: Vec<(String, u32)> = vec![Default::default(); interned.len()];
            for ((alias, chunk_id), synth) in &interned {
                v[*synth as usize] = (alias.clone(), *chunk_id);
            }
            v
        };

        // Fused ids that came from the vector half resolve by direct lookup;
        // FTS-only hits are resolved in their ORIGIN store — never "the
        // first store that answers this id".
        let by_synth: std::collections::HashMap<u32, &SourcedResult<crate::vectordb::SearchResult>> =
            vector_results
                .iter()
                .map(|hit| (interned[&(hit.alias.clone(), hit.result.id)], hit))
                .collect();

        let mut mapped: Vec<crate::vectordb::SearchResult> = Vec::new();
        for f in fused.into_iter().take(limit) {
            let Some((alias, real_id)) = origins.get(f.chunk_id as usize).cloned() else {
                continue;
            };
            let root = alias_roots
                .get(&alias)
                .map(String::as_str)
                .unwrap_or("");
            if let Some(hit) = by_synth.get(&f.chunk_id) {
                let mut r = hit.result.clone();
                r.id = real_id;
                r.score = f.rrf_score;
                r.path = prefix_path_with_alias(&r.path, Some(&alias), root);
                mapped.push(r);
            } else {
                // Chunk from FTS but not in vector results — resolve from its
                // origin store only.
                if let Some(resolved) = self
                    .resolve_chunk_in_origin(
                        &alias,
                        real_id,
                        f.rrf_score,
                        &stores,
                        aliases,
                        alias_roots,
                        &mut search_warnings,
                    )
                    .await
                {
                    mapped.push(resolved);
                }
            }
        }

        // Apply kind boost
        if let Some(target_kind) = structural_intent {
            boost_kind(&mut mapped, target_kind);
        }

        self.build_semantic_response(
            mapped,
            request,
            compact,
            !identifiers.is_empty(),
            None,
            alias_roots,
            &search_warnings,
        )
    }

    /// Resolve a fused FTS-only hit to full metadata, in its ORIGIN store.
    ///
    /// The old name ("from_stores") probed every store for the bare chunk id
    /// and kept the first answer — on a group, ids are per-repo counters, so
    /// that answered from the wrong repo and content/path attribution went to
    /// whoever was probed first, not to the repo whose FTS actually matched.
    #[allow(clippy::too_many_arguments)]
    async fn resolve_chunk_in_origin(
        &self,
        alias: &str,
        chunk_id: u32,
        score: f32,
        stores: &[Arc<SharedStores>],
        aliases: &[String],
        alias_roots: &std::collections::HashMap<String, String>,
        warnings: &mut Vec<String>,
    ) -> Option<crate::vectordb::SearchResult> {
        let idx = aliases.iter().position(|a| a == alias)?;
        let store_arc = &stores[idx];
        let store = match bounded_vector_read(&store_arc.vector_store).await {
            Ok(store) => store,
            Err(e) => {
                note_store_failure(warnings, aliases, idx, "chunk lookup", &e);
                return None;
            }
        };
        let root = alias_roots.get(alias).map(String::as_str).unwrap_or("");
        match store.get_chunk(chunk_id) {
            Ok(Some(chunk)) => Some(crate::vectordb::SearchResult {
                id: chunk_id,
                content: chunk.content,
                path: prefix_path_with_alias(&chunk.path, Some(alias), root),
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                kind: chunk.kind,
                signature: chunk.signature,
                docstring: chunk.docstring,
                context: chunk.context,
                hash: chunk.hash,
                distance: 0.0,
                score,
                context_prev: chunk.context_prev,
                context_next: chunk.context_next,
            }),
            Ok(None) => None,
            Err(ref e) => {
                note_store_failure(warnings, aliases, idx, "chunk lookup", e);
                None
            }
        }
    }

    /// Resolve origin-tagged FTS results to SearchResult, each in its own
    /// repo's store, with the alias prefix applied from the tag.
    async fn resolve_fts_to_search_results_multi(
        &self,
        fts_results: &[SourcedResult<crate::fts::FtsResult>],
        limit: usize,
        stores: &[Arc<SharedStores>],
        aliases: &[String],
        alias_roots: &std::collections::HashMap<String, String>,
        warnings: &mut Vec<String>,
    ) -> Vec<crate::vectordb::SearchResult> {
        let mut results = Vec::new();
        for fts in fts_results.iter().take(limit) {
            // `Ok(None)` means "the ORIGIN store no longer holds this chunk"
            // and is skipped; `Err` means the store is broken — collapsing
            // the two is how a dead vector store renders as an empty literal
            // search (the step-8 incident shape), so `Err` stays loud.
            let idx = match aliases.iter().position(|a| *a == fts.alias) {
                Some(idx) => idx,
                None => continue,
            };
            let store_arc = &stores[idx];
            let store = match bounded_vector_read(&store_arc.vector_store).await {
                Ok(store) => store,
                Err(e) => {
                    note_store_failure(warnings, aliases, idx, "chunk lookup", &e);
                    continue;
                }
            };
            let looked_up = store.get_chunk(fts.result.chunk_id);
            if let Err(ref e) = looked_up {
                note_store_failure(warnings, aliases, idx, "chunk lookup", e);
            }
            if let Ok(Some(chunk)) = looked_up {
                let root = alias_roots
                    .get(&fts.alias)
                    .map(String::as_str)
                    .unwrap_or("");
                results.push(crate::vectordb::SearchResult {
                    id: fts.result.chunk_id,
                    content: chunk.content,
                    path: prefix_path_with_alias(&chunk.path, Some(&fts.alias), root),
                    start_line: chunk.start_line,
                    end_line: chunk.end_line,
                    kind: chunk.kind,
                    signature: chunk.signature,
                    docstring: chunk.docstring,
                    context: chunk.context,
                    hash: chunk.hash,
                    distance: 0.0,
                    score: fts.result.score,
                    context_prev: chunk.context_prev,
                    context_next: chunk.context_next,
                });
            }
        }
        results
    }

    /// Lexical-only search: FTS without embedding service.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn semantic_search_lexical(
        &self,
        request: &SemanticSearchRequest,
        identifiers: &[String],
        limit: usize,
        compact: bool,
        stores: Option<Arc<SharedStores>>,
        project_alias: Option<&str>,
        alias_roots: &std::collections::HashMap<String, String>,
    ) -> Result<CallToolResult, McpError> {
        let structural_intent = detect_structural_intent(&request.query);

        // `project=`-scoped queries route here, not through the fan-out
        // (`is_multi` requires >1 store), so this path needs the same failure
        // reporting — it is at least as common as a group query.
        let mut lexical_warnings: Vec<String> = Vec::new();

        let mut fts_results = match self
            .with_fts_store_read_for(
                |fts_store| fts_store.search(&request.query, limit * 5, structural_intent),
                stores.clone(),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("literal search failed: {e:#}");
                tracing::error!("MCP: {}", msg);
                lexical_warnings.push(msg);
                Vec::new()
            }
        };

        // Also do exact search if identifiers detected
        for ident in identifiers {
            let exact = match self
                .with_fts_store_read_for(
                    |fts_store| fts_store.search_exact(ident, limit * 3, structural_intent),
                    stores.clone(),
                )
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let msg = format!("exact-identifier search for '{ident}' failed: {e:#}");
                    tracing::error!("MCP: {}", msg);
                    lexical_warnings.push(msg);
                    continue;
                }
            };
            merge_exact_into_fts(&mut fts_results, exact);
        }

        fts_results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Resolve FTS results to chunk metadata
        let mut results = self
            .resolve_fts_to_search_results(&fts_results, limit, stores, &mut lexical_warnings)
            .await;

        // Apply kind boost
        if let Some(target_kind) = structural_intent {
            boost_kind(&mut results, target_kind);
        }

        self.build_semantic_response(
            results,
            request,
            compact,
            !identifiers.is_empty(),
            project_alias,
            alias_roots,
            &lexical_warnings,
        )
    }

    /// Build the final SemanticSearchResponse with low-confidence signaling.
    // Eight parameters, one over clippy's threshold. Bundling them into a
    // `ResponseContext` struct is the right end state and is recorded as a
    // follow-up; doing it in an incident fix would touch all seven call sites
    // for no behavioural gain. The alternative — dropping `warnings` — is not
    // acceptable: without it a failed repo is silently reported as "no match".
    #[allow(clippy::too_many_arguments)]
    fn build_semantic_response(
        &self,
        mut results: Vec<crate::vectordb::SearchResult>,
        request: &SemanticSearchRequest,
        compact: bool,
        has_identifiers: bool,
        project_alias: Option<&str>,
        alias_roots: &std::collections::HashMap<String, String>,
        // Repos that failed during a fan-out. MUST reach the caller: the
        // consumer of this tool is a remote agent that never sees the server
        // log, so a silently omitted repo reads as "no match there" — a false
        // negative. The federated path already does this (`warnings` on the
        // remote-project fan-out); the local path never could.
        warnings: &[String],
    ) -> Result<CallToolResult, McpError> {
        let warnings = if warnings.is_empty() {
            None
        } else {
            Some(warnings.to_vec())
        };
        // `min_score` turns nearest-neighbour noise into an honest refusal:
        // weak hits are dropped before the response is built, so the empty
        // arm below explains a deliberate threshold refusal instead of
        // presenting the closest-but-irrelevant chunks as matches.
        let hits_before_min_score = results.len();
        if let Some(min_score) = request.min_score {
            results.retain(|r| r.score >= min_score);
        }
        if results.is_empty() {
            let note = match (request.min_score, hits_before_min_score) {
                (Some(min_score), n) if n > 0 => Some(format!(
                    "{n} candidate hit(s) found but all scored below min_score {min_score:.3}; \
                     refusing instead of returning the nearest neighbours."
                )),
                _ => None,
            };
            let response = SemanticSearchResponse {
                results: vec![],
                note,
                low_confidence: Some(true),
                suggested_tool: retry_hint(Some("literal_search".to_string()), &warnings),
                warnings,
            };
            let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
            return Ok(CallToolResult::success(vec![ContentBlock::text(json)]));
        }

        // Pre-compute normalized project root for stripping absolute paths
        let project_root_normalized = {
            let root = crate::cache::normalize_path_str(self.project_path.to_str().unwrap_or(""));
            root.trim_end_matches('/').to_string()
        };

        let mut items: Vec<SearchResultItem> = results
            .into_iter()
            .filter(|r| {
                if let Some(ref fp) = request.filter_path {
                    let normalized_filter = crate::cache::normalize_filter_path(fp);
                    if normalized_filter.is_empty() {
                        return true;
                    }
                    // Relativise against the ROUTED project's root, not the
                    // service's own project_path — otherwise a serve-routed
                    // absolute path never strips and every hit is dropped.
                    let filter_root = pick_filter_root(
                        &r.path,
                        project_alias,
                        alias_roots,
                        &project_root_normalized,
                    );
                    crate::cache::path_matches_filter(&r.path, &normalized_filter, &filter_root)
                } else {
                    true
                }
            })
            .map(|r| SearchResultItem {
                chunk_id: Some(r.id),
                path: r.path,
                // Chunks store 0-indexed lines; every response emits 1-based
                // editor lines, so the conversion happens once, here.
                start_line: r.start_line + 1,
                end_line: r.end_line + 1,
                kind: r.kind,
                score: r.score,
                signature: r.signature,
                content: if compact { None } else { Some(r.content) },
                context_prev: if compact { None } else { r.context_prev },
                context_next: if compact { None } else { r.context_next },
                source: None,
                chunk_ref: None,
            })
            .collect();

        // `project=` routing prefixes paths with the routed alias here. Group
        // fan-out results arrive already alias-prefixed (semantic_search_multi
        // recorded each chunk's repo), so this pass only normalizes them;
        // stdio without alias roots passes paths through untouched.
        for item in &mut items {
            if let Some(alias) = project_alias {
                if let Some(root) = alias_roots.get(alias) {
                    item.path = prefix_path_with_alias(&item.path, Some(alias), root);
                } else {
                    item.path = crate::cache::normalize_path_str(&item.path);
                }
            } else if !alias_roots.is_empty() {
                item.path = crate::cache::normalize_path_str(&item.path);
            }
        }

        // Check low-confidence: top result's RRF score below threshold
        let top_score = items.first().map(|r| r.score);
        let (low_confidence, suggested_tool) = compute_low_confidence(top_score, has_identifiers);
        let suggested_tool = retry_hint(suggested_tool, &warnings);

        let response = SemanticSearchResponse {
            results: items,
            low_confidence,
            suggested_tool,
            note: None,
            warnings,
        };

        let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
    }

    /// Resolve FTS results to SearchResult by looking up chunk metadata.
    async fn resolve_fts_to_search_results(
        &self,
        fts_results: &[crate::fts::FtsResult],
        limit: usize,
        stores: Option<Arc<SharedStores>>,
        warnings: &mut Vec<String>,
    ) -> Vec<crate::vectordb::SearchResult> {
        let outcome = self
            .with_vector_store_read_for(
                |store| {
                    let mut results = Vec::new();
                    for fts in fts_results.iter().take(limit) {
                        // A failed lookup is not an absent chunk. Propagating the
                        // error keeps a broken vector store from rendering as an
                        // ordinary empty literal search.
                        let chunk = store
                            .get_chunk(fts.chunk_id)
                            .context("Error resolving FTS hit to chunk metadata")?;
                        if let Some(chunk) = chunk {
                            results.push(crate::vectordb::SearchResult {
                                id: fts.chunk_id,
                                content: chunk.content,
                                path: chunk.path,
                                start_line: chunk.start_line,
                                end_line: chunk.end_line,
                                kind: chunk.kind,
                                signature: chunk.signature,
                                docstring: chunk.docstring,
                                context: chunk.context,
                                hash: chunk.hash,
                                distance: 0.0,
                                score: fts.score,
                                context_prev: chunk.context_prev,
                                context_next: chunk.context_next,
                            });
                        }
                    }
                    Ok(results)
                },
                stores,
            )
            .await;
        match outcome {
            Ok(results) => results,
            Err(e) => {
                let msg = format!("literal search could not read the index: {e:#}");
                tracing::error!("MCP: {}", msg);
                if !warnings.contains(&msg) {
                    warnings.push(msg);
                }
                Vec::new()
            }
        }
    }
}
