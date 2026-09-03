//! `get_chunk` tool. Extracted from `mod.rs` (todo #105) — the `#[tool]`
//! method registers through the per-module router merged in `mod.rs`'s
//! `merged_tool_router`.

use super::*;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router, ErrorData as McpError,
};

#[tool_router(router = get_chunk_router, vis = "pub(crate)")]
impl CodesearchService {
    #[tool(
        description = "Retrieve the full content of a specific chunk by its ID, plus optional surrounding lines for context.\nUse this after search or explore to read the actual code without loading the whole file.\n\nUSE FOR: reading a specific function/class body after finding it via search.\nSet context_lines (default 0, max 20) to include lines before and after the chunk.\n\nIMPORTANT (multi-repo): chunk_ids are local to each repository and are NOT globally unique.\nWhen `project` is omitted in multi-repo mode, the tool scans all repositories for the chunk_id.\nIf found in exactly one repo, it is returned automatically. If found in multiple repos, an `ambiguous_chunk_id` error lists the candidates so you can retry with `project`."
    )]
    pub(crate) async fn get_chunk(
        &self,
        Parameters(request): Parameters<GetChunkRequest>,
    ) -> Result<CallToolResult, McpError> {
        tracing::info!(
            "📥 get_chunk(chunk_id={}, project={:?})",
            request.chunk_id,
            request.project,
        );

        // Federation: a `chunk_ref` of the form "<peer>/<remote_alias>:<chunk_id>"
        // (returned by a federated search result) fetches the chunk from a remote
        // peer rather than the local index. The alias scopes the fetch to a single
        // remote project so the multi-repo peer can disambiguate the chunk_id.
        if let Some(chunk_ref) = request.chunk_ref.as_deref() {
            return self
                .federated_get_chunk(chunk_ref, request.context_lines)
                .await;
        }

        // In multi-repo serve mode, require explicit project or group scope.
        // Unscoped get_chunk would fan-out over all repos, opening all DBs unnecessarily.
        // Consistent with search/find/explore which also require scope.
        if request.project.is_none() && request.group.is_none() {
            if let Some(ref serve_state) = self.serve_state {
                let config = serve_state.config_snapshot();
                if config.repos.len() > 1 {
                    return Ok(CallToolResult::success(vec![Content::text(
                        self.format_scope_error(),
                    )]));
                }
            }
        }

        // Resolve project/group routing — allow unscoped only for single-repo mode
        let ctx = match self
            .resolve_routing(&request.project, &request.group, true, "get_chunk")
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

        let mut clamped = false;
        let mut context_lines = request.context_lines.unwrap_or(0);
        if context_lines > 20 {
            context_lines = 20;
            clamped = true;
        }

        // Stores that failed while looking up this chunk. get_chunk previously
        // collapsed every `Err` into "not found", so during the read-only
        // incident it would have reported every chunk in every vendor repo as
        // missing — a confident, wrong answer.
        let mut chunk_warnings: Vec<String> = Vec::new();

        // Look up chunk — multi-store: smart candidate detection for chunk_id collision.
        // chunk_ids are local per database, not globally unique. When no project is specified
        // and multiple stores are active, scan all stores to find which ones have this chunk_id.
        let chunk = if let Some(ref sv) = ctx.stores_vec {
            if sv.len() > 1 && request.project.is_none() {
                // Smart candidate detection: find which stores actually contain this chunk_id
                let mut candidates: Vec<(&Arc<SharedStores>, String)> = Vec::new();
                let aliases = ctx.aliases();
                for (i, store_arc) in sv.iter().enumerate() {
                    let store = store_arc.vector_store.read().await;
                    match store.get_chunk(request.chunk_id) {
                        Ok(Some(_)) => {
                            // A store that HAS the chunk stays a candidate even if
                            // its alias is missing. `resolve_repo_stores_multi`
                            // keeps stores and aliases the same length, so this is
                            // unreachable today — but gating the push on
                            // `aliases.get(i)` meant a future break of that
                            // invariant would degrade to a silent auto-route rather
                            // than a loud one. The placeholder is per-index so two
                            // aliasless candidates stay distinguishable in
                            // `candidate_projects`.
                            let alias = aliases
                                .get(i)
                                .cloned()
                                .unwrap_or_else(|| format!("<unnamed store #{i}>"));
                            candidates.push((store_arc, alias));
                        }
                        Ok(None) => continue,
                        Err(ref e) => {
                            note_store_failure(&mut chunk_warnings, aliases, i, "chunk lookup", e);
                            continue;
                        }
                    }
                }
                match candidates.len() {
                    0 => {
                        return Ok(CallToolResult::success(vec![Content::text(
                            qualify_empty_result(
                                format!(
                                    "Chunk {} not found in any repository. Verify the \
                                     chunk_id and index state.",
                                    request.chunk_id
                                ),
                                &chunk_warnings,
                            ),
                        )]));
                    }
                    1 => {
                        // Exactly one store has this chunk_id — auto-route
                        let (store_arc, ref alias) = candidates[0];
                        // Record tool call for the specific repo that served this chunk
                        if let Some(ref serve_state) = self.serve_state {
                            serve_state.record_tool_call(alias, "get_chunk");
                            serve_state.touch_access(alias);
                        }
                        let store = store_arc.vector_store.read().await;
                        match store.get_chunk(request.chunk_id) {
                            Ok(c) => c,
                            Err(ref e) => {
                                push_store_warning(
                                    &mut chunk_warnings,
                                    &store_warning(alias, "chunk lookup", &format!("{e:#}")),
                                );
                                None
                            }
                        }
                    }
                    _ => {
                        // Multiple stores have this chunk_id — ambiguous.
                        //
                        // `candidate_projects` reads as the complete list, so a
                        // store that failed to answer has to be declared: the
                        // right repo may be the one missing from it.
                        let candidate_names: Vec<&str> =
                            candidates.iter().map(|(_, a)| a.as_str()).collect();
                        let payload = ambiguous_chunk_payload(
                            request.chunk_id,
                            &candidate_names,
                            &chunk_warnings,
                        );
                        return Ok(CallToolResult::success(vec![Content::text(
                            payload.to_string(),
                        )]));
                    }
                }
            } else {
                // Single store or project specified — direct lookup
                let aliases = ctx.aliases();
                let mut found = None;
                for (i, store_arc) in sv.iter().enumerate() {
                    let store = store_arc.vector_store.read().await;
                    match store.get_chunk(request.chunk_id) {
                        Ok(Some(c)) => {
                            found = Some(c);
                            break;
                        }
                        Ok(None) => continue,
                        // Do NOT abandon the remaining stores: one broken store
                        // says nothing about the others, and the chunk may well
                        // live in a healthy one.
                        Err(ref e) => {
                            note_store_failure(&mut chunk_warnings, aliases, i, "chunk lookup", e);
                            continue;
                        }
                    }
                }
                found
            }
        } else {
            match self
                .with_vector_store_read_for(
                    |store| store.get_chunk(request.chunk_id),
                    ctx.stores.clone(),
                )
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    push_store_warning(
                        &mut chunk_warnings,
                        &store_warning(
                            ctx.project_alias.as_deref().unwrap_or("unknown"),
                            "chunk lookup",
                            &format!("{e:#}"),
                        ),
                    );
                    None
                }
            }
        };

        let mut chunk = match chunk {
            Some(c) => c,
            None => {
                return Ok(CallToolResult::success(vec![Content::text(
                    qualify_empty_result(
                        format!(
                            "Chunk {} not found. Verify the chunk_id and index state.",
                            request.chunk_id
                        ),
                        &chunk_warnings,
                    ),
                )]));
            }
        };

        // Prefix path with alias for multi-repo identification
        chunk.path = ctx.prefix_result_path(&chunk.path);

        let mut context_before = None;
        let mut context_after = None;
        let mut note = None;

        if context_lines > 0 {
            // Resolve relative chunk paths against project root (not process CWD).
            let source_path = if Path::new(&chunk.path).is_absolute() {
                PathBuf::from(&chunk.path)
            } else {
                self.project_path.join(&chunk.path)
            };
            match tokio::fs::read_to_string(&source_path).await {
                Ok(src) => {
                    let lines: Vec<&str> = src.lines().collect();
                    if !lines.is_empty() {
                        let before_start = chunk.start_line.saturating_sub(context_lines);
                        let before_end = chunk.start_line.min(lines.len());
                        if before_start < before_end {
                            context_before = Some(lines[before_start..before_end].join("\n"));
                        }

                        let after_start = chunk.end_line.min(lines.len());
                        let after_end = (chunk.end_line + context_lines).min(lines.len());
                        if after_start < after_end {
                            context_after = Some(lines[after_start..after_end].join("\n"));
                        }
                    }
                }
                Err(_) => {
                    note = Some(
                        "source file not readable, returning indexed content only".to_string(),
                    );
                }
            }
        }

        let response = GetChunkResponse {
            chunk_id: request.chunk_id,
            path: chunk.path,
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            kind: chunk.kind,
            signature: chunk.signature,
            content: chunk.content,
            context_before,
            context_after,
            context_lines_clamped: if clamped { Some(true) } else { None },
            note,
        };

        // The success path is the one that used to drop this, and it is the
        // dangerous one: a confidently-returned chunk from a group where a store
        // failed to answer looks exactly like a chunk from a healthy group. Same
        // false negative as an empty result, harder to notice.
        respond_with_object(&response, &chunk_warnings)
    }
}
