//! Consolidated `find` tool (definition/usages/imports/dependents dispatch)
//! plus its internals. Extracted from mod.rs (todo #105). The `#[tool]`
//! method registers through the per-module router that mod.rs merges inside
//! its own `merged_tool_router` composition point.

use super::*;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router, ErrorData as McpError,
};

#[tool_router(router = find_router, vis = "pub(crate)")]
impl CodesearchService {
    /// Unified symbol navigation — dispatches based on `kind`.
    #[tool(
        description = "Unified symbol navigation. Set `kind` to choose the action:\n\n- `definition` (default): locate where a symbol is defined (function, class, struct, etc.)\n- `usages`: find call-sites of a symbol via LEXICAL TEXT matching — hits may be docs/comments rather than code references; ranking puts source files first and a `note` field flags the precise upgrade path when one exists. On C#/TypeScript projects ALWAYS prefer `find_impact` for usages — it returns exact SCIP references, while this kind is only a text fallback\n- `imports`: list all imports/dependencies declared in a file (set `symbol` to the file path)\n- `dependents`: find all files that import or depend on a module, file, or symbol\n\nFor `imports`, set `symbol` to a file path. For other kinds, `symbol` is the symbol name.\n\nIMPORTANT (multi-repo): always specify either `project` (single repo) or `group` (cross-repo). Omitting both in multi-repo mode returns a `scope_required` error with the list of available projects and groups. If the user has not indicated which repository to search, ask them to choose."
    )]
    pub(crate) async fn find(
        &self,
        Parameters(request): Parameters<FindRequest>,
    ) -> Result<CallToolResult, McpError> {
        let kind = request
            .kind
            .as_deref()
            .unwrap_or("definition")
            .to_lowercase();
        tracing::info!(
            "📥 find(symbol={:?}, kind={}, project={:?}, group={:?})",
            request.symbol,
            kind,
            request.project,
            request.group,
        );
        match kind.as_str() {
            "definition" => {
                let def_req = FindDefinitionRequest {
                    symbol: request.symbol,
                    kind: request.definition_kind,
                    limit: request.limit,
                    project: request.project,
                    group: request.group,
                };
                self.find_definition(Parameters(def_req)).await
            }
            "usages" => {
                let usages_req = FindUsagesRequest {
                    symbol: request.symbol,
                    limit: request.limit,
                    project: request.project,
                    group: request.group,
                };
                self.find_usages(Parameters(usages_req)).await
            }
            "imports" => {
                let imports_req = FindImportsRequest {
                    path: request.symbol,
                    project: request.project,
                    group: request.group,
                };
                self.find_imports(Parameters(imports_req)).await
            }
            "dependents" => {
                let dep_req = FindDependentsRequest {
                    symbol_or_path: request.symbol,
                    limit: request.limit,
                    project: request.project,
                    group: request.group,
                };
                self.find_dependents(Parameters(dep_req)).await
            }
            _ => Ok(CallToolResult::success(vec![Content::text(format!(
                "Unknown find kind '{}'. Use `definition`, `usages`, `imports`, or `dependents`.",
                kind
            ))])),
        }
    }

    // === find_definition internal ===

    /// Internal: find symbol definitions, used by `find(kind="definition")`.
    async fn find_definition(
        &self,
        Parameters(request): Parameters<FindDefinitionRequest>,
    ) -> Result<CallToolResult, McpError> {
        let limit = request.limit.unwrap_or(20);

        tracing::debug!(
            "MCP find_definition: symbol='{}', kind={:?}, limit={}",
            request.symbol,
            request.kind,
            limit
        );

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

        // Stores that failed during this lookup. Without this, "the symbol may
        // not be indexed" below is emitted as a confident diagnosis even when
        // no store ever answered.
        let mut find_warnings: Vec<String> = Vec::new();

        // FTS search — multi-store or single
        let fts_results = if let Some(ref sv) = ctx.stores_vec {
            let sa = ctx.store_aliases.as_ref().unwrap();
            self.with_fts_store_read_multi(
                |fts_store| fts_store.search(&request.symbol, limit * 3, None),
                sv.clone(),
                sa,
            )
            .await
            .unwrap_or_default()
            .into_results(&mut find_warnings, "definition search")
        } else {
            match self
                .with_fts_store_read_for(
                    |fts_store| fts_store.search(&request.symbol, limit * 3, None),
                    ctx.stores.clone(),
                )
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error searching: {e:#}"
                    ))]));
                }
            }
        };

        if fts_results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                qualify_empty_result(
                    format!(
                        "No definition found for '{}'. The symbol may not be indexed.",
                        request.symbol
                    ),
                    &find_warnings,
                ),
            )]));
        }

        // Resolve chunk metadata and filter by definition kinds
        let requested_kind = request.kind.clone();
        let mut items: Vec<ReferenceItem> = if let Some(ref sv) = ctx.stores_vec {
            let aliases = ctx.aliases();
            let mut items: Vec<ReferenceItem> = Vec::new();
            'outer: for fts_result in &fts_results {
                for (store_idx, store_arc) in sv.iter().enumerate() {
                    let store = store_arc.vector_store.read().await;
                    let looked_up = store.get_chunk(fts_result.chunk_id);
                    if let Err(ref e) = looked_up {
                        // `Ok(None)` = chunk not in this store (normal during
                        // fan-out); `Err` = broken store. Skipping the `Err`
                        // silently made a dead store look like "symbol not
                        // found" — carry it in the warnings channel instead.
                        note_store_failure(
                            &mut find_warnings,
                            aliases,
                            store_idx,
                            "chunk lookup",
                            e,
                        );
                    }
                    if let Ok(Some(chunk)) = looked_up {
                        // Skip non-definition kinds — try next FTS result, not next store
                        if !DEFINITION_KINDS.contains(&chunk.kind.as_str()) {
                            continue 'outer;
                        }
                        if let Some(ref rk) = requested_kind {
                            if chunk.kind != *rk {
                                continue 'outer;
                            }
                        }
                        items.push(ReferenceItem {
                            chunk_id: fts_result.chunk_id,
                            path: chunk.path,
                            line: chunk.start_line,
                            kind: chunk.kind,
                            signature: chunk.signature,
                            score: fts_result.score,
                        });
                        if items.len() >= limit {
                            break 'outer;
                        }
                        break; // Found in this store — move to next FTS result
                    }
                }
                // If we get here, the chunk was Ok(None) in every store (not
                // held anywhere — skip it) or its lookups failed (noted in
                // find_warnings above).
            }
            items
        } else {
            match self
                .with_vector_store_read_for(
                    |store| {
                        // Resolve chunk metadata first: a store `Err` must
                        // reach the error arm below ("Error opening database")
                        // instead of masquerading as a non-definition or
                        // missing chunk — `Ok(None)` alone is a true miss.
                        let resolved: anyhow::Result<Vec<_>> = fts_results
                            .iter()
                            .map(|fts_result| {
                                let chunk = store.get_chunk(fts_result.chunk_id)?;
                                Ok((chunk, fts_result.chunk_id, fts_result.score))
                            })
                            .collect();
                        let items = resolved?
                            .into_iter()
                            .filter_map(|(looked_up, chunk_id, score)| {
                                let chunk = looked_up?;
                                if !DEFINITION_KINDS.contains(&chunk.kind.as_str()) {
                                    return None;
                                }
                                if let Some(ref requested_kind) = requested_kind {
                                    if chunk.kind != *requested_kind {
                                        return None;
                                    }
                                }
                                Some(ReferenceItem {
                                    chunk_id,
                                    path: chunk.path,
                                    line: chunk.start_line,
                                    kind: chunk.kind,
                                    signature: chunk.signature,
                                    score,
                                })
                            })
                            .take(limit)
                            .collect();
                        Ok(items)
                    },
                    ctx.stores.clone(),
                )
                .await
            {
                Ok(items) => items,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error opening database: {e:#}"
                    ))]));
                }
            }
        };

        // Prefix paths with alias for multi-repo identification
        for item in &mut items {
            item.path = ctx.prefix_result_path(&item.path);
        }

        respond_with_items(&items, &find_warnings, || {
            format!(
                "No definition found for '{}'. Try find_usages() to find references, \
                 or broaden your search.",
                request.symbol
            )
        })
    }

    // === find_usages tool ===

    async fn find_usages(
        &self,
        Parameters(request): Parameters<FindUsagesRequest>,
    ) -> Result<CallToolResult, McpError> {
        self.find_usages_impl(
            request.symbol.clone(),
            request.limit.unwrap_or(20),
            request.project,
            request.group,
        )
        .await
    }

    /// Shared implementation for find_usages (used by `find(kind="usages")`).
    async fn find_usages_impl(
        &self,
        symbol: String,
        limit: usize,
        project: Option<String>,
        group: Option<String>,
    ) -> Result<CallToolResult, McpError> {
        tracing::debug!("MCP find_usages: symbol='{}', limit={}", symbol, limit);

        // Resolve project/group routing
        let ctx = match self.resolve_routing(&project, &group, false, "find").await {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        // See `find_definition`: an empty result and a dead store must not
        // produce the same sentence.
        let mut find_warnings: Vec<String> = Vec::new();

        // FTS search — multi-store or single
        let fts_results = if let Some(ref sv) = ctx.stores_vec {
            let sa = ctx.store_aliases.as_ref().unwrap();
            self.with_fts_store_read_multi(
                |fts_store| fts_store.search(&symbol, limit * 2, None),
                sv.clone(),
                sa,
            )
            .await
            .unwrap_or_default()
            .into_results(&mut find_warnings, "usage search")
        } else {
            match self
                .with_fts_store_read_for(
                    |fts_store| fts_store.search(&symbol, limit * 2, None),
                    ctx.stores.clone(),
                )
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error searching: {e:#}"
                    ))]));
                }
            }
        };

        if fts_results.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                qualify_empty_result(
                    format!("No usages found for '{symbol}'. The symbol may not be indexed."),
                    &find_warnings,
                ),
            )]));
        }

        // Resolve chunks and exclude definition chunks
        let mut items: Vec<ReferenceItem> = if let Some(ref sv) = ctx.stores_vec {
            let aliases = ctx.aliases();
            let mut items: Vec<ReferenceItem> = Vec::new();
            for fts_result in &fts_results {
                for (store_idx, store_arc) in sv.iter().enumerate() {
                    let store = store_arc.vector_store.read().await;
                    let looked_up = store.get_chunk(fts_result.chunk_id);
                    if let Err(ref e) = looked_up {
                        // Same rule as find_definition: `Err` is a broken
                        // store, not "no usages" — carry it in the channel.
                        note_store_failure(
                            &mut find_warnings,
                            aliases,
                            store_idx,
                            "chunk lookup",
                            e,
                        );
                    }
                    if let Ok(Some(chunk)) = looked_up {
                        if !is_definition_chunk(&chunk.kind, &chunk.signature, &symbol) {
                            items.push(ReferenceItem {
                                chunk_id: fts_result.chunk_id,
                                path: chunk.path,
                                line: chunk.start_line,
                                kind: chunk.kind,
                                signature: chunk.signature,
                                score: fts_result.score,
                            });
                        }
                        break;
                    }
                }
            }
            items
        } else {
            match self
                .with_vector_store_read_for(
                    |store| {
                        // Resolve first: a store `Err` must reach the error
                        // arm below instead of masquerading as a definition
                        // chunk or a miss — `Ok(None)` alone is a true miss.
                        let resolved: anyhow::Result<Vec<_>> = fts_results
                            .iter()
                            .map(|fts_result| {
                                let chunk = store.get_chunk(fts_result.chunk_id)?;
                                Ok((chunk, fts_result.chunk_id, fts_result.score))
                            })
                            .collect();
                        let items = resolved?
                            .into_iter()
                            .filter_map(|(looked_up, chunk_id, score)| {
                                let chunk = looked_up?;
                                if is_definition_chunk(&chunk.kind, &chunk.signature, &symbol) {
                                    return None;
                                }
                                Some(ReferenceItem {
                                    chunk_id,
                                    path: chunk.path,
                                    line: chunk.start_line,
                                    kind: chunk.kind,
                                    signature: chunk.signature,
                                    score,
                                })
                            })
                            .collect();
                        Ok(items)
                    },
                    ctx.stores.clone(),
                )
                .await
            {
                Ok(items) => items,
                Err(e) => {
                    return Ok(CallToolResult::success(vec![Content::text(format!(
                        "Error opening database: {e:#}"
                    ))]));
                }
            }
        };

        // Prefix paths with alias for multi-repo identification
        for item in &mut items {
            item.path = ctx.prefix_result_path(&item.path);
        }

        // Lexical FTS ranks docs, comments and code by the same text score,
        // so a usages query can bury the real call-sites under markdown. The
        // cut to `limit` MUST happen after the re-order: BM25 systematically
        // scores short markdown blocks above long source files (document
        // length normalisation), so ranking after a `take(limit)` would have
        // nothing left to reorder exactly when it matters most. Stable sort
        // preserves score order within each group; nothing is filtered.
        rank_code_first(&mut items);
        items.truncate(limit);

        // When the hits include SCIP-backed source files and a precise
        // backend is installed, tell the agent the exact upgrade path —
        // otherwise the lexical list silently stands in for real references.
        let note = scip_usages_note(&self.symbol_registry, &items, &symbol);

        respond_with_items_noted(&items, &find_warnings, note.as_deref(), || {
            format!(
                "No usages found for '{symbol}' (only definitions were found). Try \
                 find_definition() to locate the declaration."
            )
        })
    }
}
