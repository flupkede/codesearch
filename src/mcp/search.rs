//! Consolidated `search` tool (semantic/lexical dispatch) + the semantic
//! search machinery it drives. Extracted from `mod.rs` (todo #105) — the
//! `#[tool]` method registers through the per-module router merged in
//! `mod.rs`'s `merged_tool_router`.

use super::*;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router, ErrorData as McpError,
};

#[tool_router(router = search_router, vis = "pub(crate)")]
impl CodesearchService {
    // Consolidated tools (the primary 5-tool surface)
    // ─────────────────────────────────────────────────────────────────

    /// Unified search tool — dispatches to semantic or literal search based on `mode`.
    #[tool(
        description = "Unified code search. Set `mode` to choose the backend:\n\n- `semantic` (default): vector embeddings + BM25 FTS + exact-identifier boosting, fused with RRF. Best for conceptual queries, identifier lookups, and mixed natural-language + symbol queries.\n- `literal`: pure FTS, no embeddings. Fast and works without an embedding model. Sub-mode selection:\n  * Queries with operators, brackets, or punctuation (`foo = null`, `Vec<T>`, `return x;`, `a::b`) -> set `regex=true` and write the query as a regex. BM25 tokenizes on punctuation otherwise, producing noisy results.\n  * Multi-word exact phrases -> set `phrase=true`.\n  * Plain identifier lookups (`CodesearchService`) -> leave both false.\n\nFor semantic mode, optionally set `semantic_mode`: \"auto\" (default) | \"semantic\" | \"lexical\" | \"hybrid\".\nReturns metadata only by default (`compact=true`). Use `get_chunk` to read full code. Prefer `search(mode=\"literal\", regex=true)` over external grep/ripgrep for code patterns.\n\nIMPORTANT (multi-repo): always specify either `project` (single repo) or `group` (cross-repo). Omitting both in multi-repo mode returns a `scope_required` error with the list of available projects and groups. If the user has not indicated which repository to search, ask them to choose."
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
            _ => Ok(CallToolResult::success(vec![Content::text(format!(
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
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
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
                return Ok(CallToolResult::success(vec![Content::text(e)]));
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
        let query_embedding = {
            let mut service_guard = match self.get_embedding_service() {
                Ok(g) => g,
                Err(e) => {
                    tracing::error!("MCP: Failed to get embedding service: {:?}", e);
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error initializing embedding service: {e:#}"
                    ))]));
                }
            };

            let service = service_guard.as_mut().unwrap();
            tracing::debug!("MCP: Embedding query...");
            match service.embed_query(&request.query) {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!("MCP: Failed to embed query: {:?}", e);
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error embedding query: {e:#}"
                    ))]));
                }
            }
        };

        // Failures on this single-store path. The group fan-out has carried a
        // warnings channel since the read-only incident; without the same thing
        // here, `project=<alias>` — the form an agent uses most — still reports
        // a broken store as an ordinary empty result.
        let mut single_warnings: Vec<String> = Vec::new();

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
                    return Ok(CallToolResult::success(vec![Content::text(format!(
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
                merge_exact_into_fts(&mut all_fts, exact_outcome.results);
            }

            all_fts.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            let results = self
                .resolve_fts_to_search_results_multi(
                    &all_fts,
                    limit,
                    &stores,
                    aliases,
                    &mut lexical_warnings,
                )
                .await;

            if let Some(target_kind) = structural_intent {
                // We need mutable results but we have them as vectordb::SearchResult
                let mut mutable_results = results;
                boost_kind(&mut mutable_results, target_kind);
                return self.build_semantic_response(
                    mutable_results,
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
        let query_embedding = {
            let mut service_guard = match self.get_embedding_service() {
                Ok(g) => g,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error initializing embedding service: {e:#}"
                    ))]));
                }
            };
            let service = service_guard.as_mut().unwrap();
            match service.embed_query(&request.query) {
                Ok(e) => e,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error embedding query: {e:#}"
                    ))]));
                }
            }
        };

        // Search vector stores across all repos
        let outcome = self
            .with_vector_store_read_multi(
                |store| {
                    store
                        .search(&query_embedding, limit * 5)
                        .context("Error searching vector store")
                },
                stores.clone(),
                aliases,
            )
            .await;

        // Warnings raised by the fan-out, carried into the response so the
        // calling agent can tell "not in the corpus" from "that repo is down".
        let mut search_warnings: Vec<String> = Vec::new();

        let vector_results =
            match outcome {
                Ok(o) => {
                    if !o.failures.is_empty() {
                        tracing::error!(
                            "MCP: vector fan-out degraded — {} of {} repo(s) failed: {:?}",
                            o.failures.len(),
                            stores.len(),
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
                            return Ok(CallToolResult::success(vec![Content::text(format!(
                                "Error searching vector store: {} of {} repo(s) in scope failed \
                             and none returned results:\n{}",
                                o.failures.len(),
                                stores.len(),
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
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error searching vector store: {e:#}"
                    ))]));
                }
            };

        // === Mode: "semantic" — vector only ===
        if mode == "semantic" {
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
            let mut exact_results: Vec<crate::fts::FtsResult> = Vec::new();
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
                    if !exact_results.iter().any(|e| e.chunk_id == r.chunk_id) {
                        exact_results.push(r);
                    }
                }
            }
            exact_results
        } else {
            Vec::new()
        };

        // RRF fusion
        let fused = if identifiers.is_empty() {
            rrf_fusion(&vector_results, &fts_results, vector_k as f32)
        } else {
            rrf_fusion_with_exact(
                &vector_results,
                &fts_results,
                &all_exact,
                vector_k as f32,
                fts_k as f32,
                EXACT_MATCH_RRF_K,
            )
        };

        // Map FusedResult back to SearchResult via chunk lookup across all stores
        let chunk_to_result: std::collections::HashMap<u32, &crate::vectordb::SearchResult> =
            vector_results.iter().map(|r| (r.id, r)).collect();

        let mut mapped: Vec<crate::vectordb::SearchResult> = Vec::new();
        for f in fused.into_iter().take(limit) {
            if let Some(result) = chunk_to_result.get(&f.chunk_id) {
                let mut r = (*result).clone();
                r.score = f.rrf_score;
                mapped.push(r);
            } else {
                // Chunk from FTS but not in vector results — resolve from stores
                if let Some(resolved) = self
                    .resolve_chunk_from_stores(
                        f.chunk_id,
                        f.rrf_score,
                        &stores,
                        aliases,
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

    /// Resolve a single chunk from multiple stores (used for FTS-only hits in multi-store fusion).
    async fn resolve_chunk_from_stores(
        &self,
        chunk_id: u32,
        score: f32,
        stores: &[Arc<SharedStores>],
        aliases: &[String],
        warnings: &mut Vec<String>,
    ) -> Option<crate::vectordb::SearchResult> {
        for (idx, store_arc) in stores.iter().enumerate() {
            let store = store_arc.vector_store.read().await;
            let looked_up = store.get_chunk(chunk_id);
            if let Err(ref e) = looked_up {
                note_store_failure(warnings, aliases, idx, "chunk lookup", e);
            }
            if let Ok(Some(chunk)) = looked_up {
                return Some(crate::vectordb::SearchResult {
                    id: chunk_id,
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
                    score,
                    context_prev: chunk.context_prev,
                    context_next: chunk.context_next,
                });
            }
        }
        None
    }

    /// Resolve FTS results to SearchResult using multiple stores.
    async fn resolve_fts_to_search_results_multi(
        &self,
        fts_results: &[crate::fts::FtsResult],
        limit: usize,
        stores: &[Arc<SharedStores>],
        aliases: &[String],
        warnings: &mut Vec<String>,
    ) -> Vec<crate::vectordb::SearchResult> {
        let mut results = Vec::new();
        for fts in fts_results.iter().take(limit) {
            for (idx, store_arc) in stores.iter().enumerate() {
                let store = store_arc.vector_store.read().await;
                let looked_up = store.get_chunk(fts.chunk_id);
                if let Err(ref e) = looked_up {
                    // `Ok(None)` means "this store does not hold that chunk" and
                    // is normal during fan-out; `Err` means the store is broken.
                    // Collapsing the two is how a dead vector store renders as
                    // an empty literal search — the exact shape of the step-8
                    // incident, which tantivy-side checks cannot detect.
                    note_store_failure(warnings, aliases, idx, "chunk lookup", e);
                }
                if let Ok(Some(chunk)) = looked_up {
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
                    break; // Found in this store, skip remaining stores
                }
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
        results: Vec<crate::vectordb::SearchResult>,
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
        if results.is_empty() {
            let response = SemanticSearchResponse {
                results: vec![],
                low_confidence: Some(true),
                suggested_tool: retry_hint(Some("literal_search".to_string()), &warnings),
                warnings,
            };
            let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
            return Ok(CallToolResult::success(vec![Content::text(json)]));
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
                start_line: r.start_line,
                end_line: r.end_line,
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

        // Prefix paths with alias for multi-repo / single-project identification
        for item in &mut items {
            if let Some(alias) = project_alias {
                if let Some(root) = alias_roots.get(alias) {
                    item.path = prefix_path_with_alias(&item.path, Some(alias), root);
                } else {
                    item.path = crate::cache::normalize_path_str(&item.path);
                }
            } else if !alias_roots.is_empty() {
                item.path = prefix_path_multi(&item.path, &[], alias_roots);
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
            warnings,
        };

        let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
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
