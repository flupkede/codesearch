//! Consolidated `explore` tool (outline/similar dispatch) + the outline
//! internals. Extracted from `mod.rs` (todo #105) — the `#[tool]` method
//! registers through the per-module router merged in `mod.rs`'s
//! `merged_tool_router`.

use super::*;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router, ErrorData as McpError,
};

#[tool_router(router = explore_router, vis = "pub(crate)")]
impl CodesearchService {
    /// Unified exploration tool — dispatches based on `kind`.
    #[tool(
        description = "Unified code exploration. Set `kind` to choose the action:\n\n- `outline` (default): list all indexed top-level symbols in a file — kind, signature, and line range. Set `target` to a file path.\n- `similar`: find chunks semantically similar to a given chunk by its ID. Set `target` to the chunk_id (as string).\n\nIMPORTANT (multi-repo): always specify either `project` (single repo) or `group` (cross-repo). Omitting both in multi-repo mode returns a `scope_required` error with the list of available projects and groups. If the user has not indicated which repository to search, ask them to choose."
    )]
    pub(crate) async fn explore(
        &self,
        Parameters(request): Parameters<ExploreRequest>,
    ) -> Result<CallToolResult, McpError> {
        let kind = request.kind.as_deref().unwrap_or("outline").to_lowercase();
        tracing::info!(
            "📥 explore(target={:?}, kind={}, project={:?})",
            request.target,
            kind,
            request.project,
        );
        match kind.as_str() {
            "outline" => {
                let outline_req = FileOutlineRequest {
                    path: request.target,
                    project: request.project,
                    group: request.group,
                };
                self.file_outline(Parameters(outline_req)).await
            }
            "similar" => {
                let chunk_id = match request.target.parse::<u32>() {
                    Ok(id) => id,
                    Err(_) => {
                        return Ok(CallToolResult::success(vec![Content::text(format!(
                            "For similar mode, `target` must be a numeric chunk_id, got: '{}'",
                            request.target
                        ))]));
                    }
                };
                let similar_req = SimilarChunksRequest {
                    chunk_id,
                    limit: request.limit,
                    project: request.project,
                    group: request.group,
                };
                self.similar_chunks(Parameters(similar_req)).await
            }
            _ => Ok(CallToolResult::success(vec![Content::text(format!(
                "Unknown explore kind '{}'. Use `outline` or `similar`.",
                kind
            ))])),
        }
    }

    /// Fetch outline items for an already-normalised absolute path.
    ///
    /// Returns `Ok(vec![])` when no chunks match.
    /// In multi-store mode, per-store I/O failures are recorded in `warnings` and
    /// skipped (never `Err`) so one broken repo cannot blank the whole outline.
    /// In single-store mode, I/O failures are returned as `Err`.
    ///
    /// `warnings` is not optional: without it a failed store is indistinguishable
    /// from a file with no indexed chunks, and the caller is told the file is not
    /// indexed — a diagnosis, and a wrong one.
    async fn outline_items_for_normalized(
        &self,
        normalized: &str,
        ctx: &MultiStoreContext,
        warnings: &mut Vec<String>,
    ) -> anyhow::Result<Vec<FileOutlineItem>> {
        if let Some(ref sv) = ctx.stores_vec {
            let aliases = ctx.aliases();
            let mut all_items: Vec<FileOutlineItem> = Vec::new();
            let mut seen_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
            for (store_idx, store_arc) in sv.iter().enumerate() {
                let store = store_arc.vector_store.read().await;
                match store.chunks_for_file(normalized) {
                    Ok(metas) => {
                        for c in metas {
                            if seen_ids.insert(c.id) {
                                all_items.push(FileOutlineItem {
                                    chunk_id: c.id,
                                    kind: c.kind,
                                    signature: c.signature,
                                    start_line: c.start_line,
                                    end_line: c.end_line,
                                });
                            }
                        }
                    }
                    Err(ref e) => {
                        note_store_failure(warnings, aliases, store_idx, "outline scan", e);
                    }
                }
            }
            all_items.sort_by_key(|i| i.start_line);
            Ok(all_items)
        } else {
            let normalized_owned = normalized.to_string();
            self.with_vector_store_read_for(
                move |store| {
                    let mut out: Vec<FileOutlineItem> = store
                        .chunks_for_file(&normalized_owned)?
                        .into_iter()
                        .map(|c| FileOutlineItem {
                            chunk_id: c.id,
                            kind: c.kind,
                            signature: c.signature,
                            start_line: c.start_line,
                            end_line: c.end_line,
                        })
                        .collect();
                    out.sort_by_key(|i| i.start_line);
                    Ok(out)
                },
                ctx.stores.clone(),
            )
            .await
        }
    }

    async fn file_outline(
        &self,
        Parameters(request): Parameters<FileOutlineRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve project/group routing
        let ctx = match self
            .resolve_routing(&request.project, &request.group, false, "explore")
            .await
        {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        // Outline operates on a single repo — reject group fan-out
        if ctx.is_multi {
            return Ok(CallToolResult::success(vec![Content::text(
                "Tool 'explore' operates on a single repo. Use 'project' instead of 'group'."
                    .to_string(),
            )]));
        }

        if ctx.needs_local_db {
            if let Err(e) = self.ensure_database_exists() {
                return Ok(CallToolResult::success(vec![Content::text(e)]));
            }
        }

        // In serve mode, use the resolved project root from alias_roots;
        // self.project_path is "serve://multi-repo" which doesn't resolve.
        let project_root = if let Some(ref alias) = ctx.project_alias {
            ctx.alias_roots
                .get(alias)
                .map(PathBuf::from)
                .unwrap_or_else(|| self.project_path.clone())
        } else {
            self.project_path.clone()
        };
        // Strip project-alias prefix from target path if present.
        // E.g. "ExampleRepo/src/foo.cs" with project="ExampleRepo" → "src/foo.cs"
        let stripped_path = strip_alias_prefix(&request.path, ctx.project_alias.as_ref());
        let normalized = normalize_tool_path(&stripped_path, &project_root);

        let mut outline_warnings: Vec<String> = Vec::new();
        let mut items = match self
            .outline_items_for_normalized(&normalized, &ctx, &mut outline_warnings)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Error reading outline: {e:#}"
                ))]));
            }
        };

        // Two-pass fallback: if alias-stripping changed the path and yielded no results,
        // try the original un-stripped path. Handles the case where the project alias
        // matches a package subdirectory name (e.g. project "my_pkg" with target
        // "my_pkg/config.py" → after strip becomes "config.py" which is wrong;
        // the correct relative path is "my_pkg/config.py").
        if items.is_empty() && stripped_path != request.path {
            let normalized_orig = normalize_tool_path(&request.path, &project_root);
            if normalized_orig != normalized {
                tracing::debug!(
                    "file_outline: primary '{}' empty, trying fallback '{}'",
                    normalized,
                    normalized_orig
                );
                items = match self
                    .outline_items_for_normalized(&normalized_orig, &ctx, &mut outline_warnings)
                    .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            "file_outline: fallback '{}' also failed: {:?}",
                            normalized_orig,
                            e
                        );
                        push_store_warning(
                            &mut outline_warnings,
                            &store_warning(
                                ctx.project_alias.as_deref().unwrap_or("unknown"),
                                "outline scan",
                                &format!("{e:#}"),
                            ),
                        );
                        Vec::new()
                    }
                };
            }
        }

        respond_with_items(&items, &outline_warnings, || {
            "No indexed chunks found for path. Verify the file is within the \
             project root and the index is up to date."
                .to_string()
        })
    }
}
