//! Consolidated `status` tool (index/projects dispatch) + the status
//! internals. Extracted from `mod.rs` (todo #105) — the `#[tool]` method
//! registers through the per-module router merged in `mod.rs`'s
//! `merged_tool_router`.

use super::*;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router, ErrorData as McpError,
};

#[tool_router(router = status_router, vis = "pub(crate)")]
impl CodesearchService {
    /// Unified status tool — dispatches based on `kind`.
    #[tool(
        description = "Unified status/info tool. Set `kind` to choose the action:\n\n- `index` (default): get the status of the local search index (model info, chunk count, readiness)\n- `projects`: list all registered projects/repositories, groups, and their index status"
    )]
    pub(crate) async fn status(
        &self,
        Parameters(request): Parameters<StatusRequest>,
    ) -> Result<CallToolResult, McpError> {
        let kind = request.kind.as_deref().unwrap_or("index").to_lowercase();
        tracing::info!("📥 status(kind={})", kind);
        match kind.as_str() {
            "index" => self.index_status_impl(request.project, request.group).await,
            "projects" => self.list_projects().await,
            _ => Ok(CallToolResult::success(vec![Content::text(format!(
                "Unknown status kind '{}'. Use `index` or `projects`.",
                kind
            ))])),
        }
    }

    // ─────────────────────────────────────────────────────────────────
    /// Internal implementation for index_status with optional project/group routing.
    async fn index_status_impl(
        &self,
        project: Option<String>,
        group: Option<String>,
    ) -> Result<CallToolResult, McpError> {
        // When no project/group specified in serve mode, return lightweight aggregated
        // status WITHOUT opening any databases. Only a specific project/group request
        // should trigger DB activation.
        if project.is_none() && group.is_none() {
            if let Some(ref serve_state) = self.serve_state {
                let config = serve_state.config_snapshot();
                let repo_count = config.repos.len();
                // Count the virtual "all" group when repos are registered, so the
                // summary doesn't read "0 group(s)" while `all` is actually available.
                let group_count = config.groups.len() + if config.repos.is_empty() { 0 } else { 1 };
                let statuses = serve_state.repo_statuses_lightweight();
                let open_count = statuses
                    .iter()
                    .filter(|(_, r)| matches!(r.status, crate::serve::RepoStateLabel::Open))
                    .count();
                let warm_count = statuses
                    .iter()
                    .filter(|(_, r)| matches!(r.status, crate::serve::RepoStateLabel::Warm))
                    .count();
                let closed_count = statuses
                    .iter()
                    .filter(|(_, r)| matches!(r.status, crate::serve::RepoStateLabel::Closed))
                    .count();

                let status = if open_count + warm_count > 0 {
                    "ready".to_string()
                } else if repo_count > 0 {
                    "idle".to_string()
                } else {
                    "no_repos".to_string()
                };

                let status_message = format!(
                    "{} repo(s) registered, {} group(s). Open: {}, Warm: {}, Closed: {}.",
                    repo_count, group_count, open_count, warm_count, closed_count
                );

                let response = IndexStatusResponse {
                    indexed: open_count + warm_count > 0,
                    status,
                    status_message,
                    total_chunks: 0, // Not available without opening DBs
                    total_files: 0,
                    model: self.model_type.short_name().to_string(),
                    dimensions: 0,
                    max_chunk_id: 0,
                    db_path: format!("({} repos)", repo_count),
                    project_path: format!("serve mode — {} repo(s)", repo_count),
                    error_message: None,
                    mode: self.mcp_mode(),
                };

                let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
                return Ok(CallToolResult::success(vec![Content::text(json)]));
            }
        }

        // Resolve project/group routing — status is scope-free, allow unscoped fan-out
        let ctx = match self.resolve_routing(&project, &group, true, "status").await {
            Ok(c) => c,
            Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
        };

        if ctx.needs_local_db {
            let indexed = self.db_path.exists();

            if !indexed {
                let response = IndexStatusResponse {
                    indexed: false,
                    status: "not_indexed".to_string(),
                    status_message: "No index found. Run 'codesearch index' or start with --create-index=true to automatically create one.".to_string(),
                    total_chunks: 0,
                    total_files: 0,
                    model: "none".to_string(),
                    dimensions: 0,
                    max_chunk_id: 0,
                    db_path: self.db_path.display().to_string(),
                    project_path: self.project_path.display().to_string(),
                    error_message: None,
                    mode: self.mcp_mode(),
                };
                let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
                return Ok(CallToolResult::success(vec![Content::text(json)]));
            }
        }

        if let Some(ref sv) = ctx.stores_vec {
            // Multi-store: aggregate stats across all group members
            let mut total_chunks = 0usize;
            let mut total_files = 0usize;
            let mut max_chunk_id = 0u32;
            let mut dimensions = 0usize;
            let mut all_indexed = true;
            let aliases = ctx.aliases();
            let mut stats_warnings: Vec<String> = Vec::new();
            let mut failed_count = 0usize;

            for (i, store_arc) in sv.iter().enumerate() {
                let store = store_arc.vector_store.read().await;
                match store.stats() {
                    Ok(stats) => {
                        total_chunks += stats.total_chunks;
                        total_files += stats.total_files;
                        if stats.max_chunk_id > max_chunk_id {
                            max_chunk_id = stats.max_chunk_id;
                        }
                        if stats.dimensions > 0 {
                            dimensions = stats.dimensions;
                        }
                        if !stats.indexed {
                            all_indexed = false;
                        }
                    }
                    // `all_indexed = false` alone renders identically to "still
                    // warming" — the caller has no way to tell "wait" from "this
                    // store is down". This is the tool whose job is reporting index
                    // health, so it must not stay silent on the one signal that
                    // matters here: bind the error, carry it, never `Err(_)`.
                    Err(ref e) => {
                        all_indexed = false;
                        failed_count += 1;
                        note_store_failure(&mut stats_warnings, aliases, i, "stats", e);
                    }
                }
            }

            let (status, status_message) =
                index_status_summary(sv.len(), failed_count, total_chunks);

            let response = IndexStatusResponse {
                indexed: all_indexed,
                status,
                status_message,
                total_chunks,
                total_files,
                model: self.model_type.short_name().to_string(),
                dimensions,
                max_chunk_id,
                db_path: format!("({} repos)", sv.len()),
                project_path: format!("group with {} repo(s)", sv.len()),
                error_message: None,
                mode: self.mcp_mode(),
            };

            return respond_with_object(&response, &stats_warnings);
        }

        // Single-store path
        let stats = match self
            .with_vector_store_read_for(
                |store| store.stats().context("Error getting index stats"),
                ctx.stores.clone(),
            )
            .await
        {
            Ok(s) => s,
            Err(e) => {
                let response = IndexStatusResponse {
                    indexed: false,
                    status: "error".to_string(),
                    status_message: format!("{}", e),
                    total_chunks: 0,
                    total_files: 0,
                    model: self.model_type.short_name().to_string(),
                    dimensions: 0,
                    max_chunk_id: 0,
                    db_path: self.db_path.display().to_string(),
                    project_path: self.project_path.display().to_string(),
                    error_message: Some(format!("{}", e)),
                    mode: self.mcp_mode(),
                };
                let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
                return Ok(CallToolResult::success(vec![Content::text(json)]));
            }
        };

        // Determine status based on database state
        let (status, status_message) = if stats.total_chunks == 0 {
            (
                "building".to_string(),
                "Index is being built in the background. Searches may fail until indexing completes. Please check back in a few minutes.".to_string(),
            )
        } else {
            (
                "ready".to_string(),
                "Index is ready for searching.".to_string(),
            )
        };

        let response = IndexStatusResponse {
            indexed: stats.indexed,
            status,
            status_message,
            total_chunks: stats.total_chunks,
            total_files: stats.total_files,
            model: self.model_type.short_name().to_string(),
            dimensions: stats.dimensions,
            max_chunk_id: stats.max_chunk_id,
            db_path: self.db_path.display().to_string(),
            project_path: self.project_path.display().to_string(),
            error_message: None,
            mode: self.mcp_mode(),
        };

        let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// List all registered projects and groups. Called by `status(kind="projects")`.
    /// Build the `remote_projects` listing (opt-in mounts) for `list_projects`.
    fn remote_projects_listing(
        config: &crate::db_discovery::repos::ReposConfig,
    ) -> Vec<RemoteProjectInfo> {
        config
            .mounted_remote_projects()
            .into_iter()
            .filter_map(|(name, target)| match target {
                crate::db_discovery::repos::Target::RemoteProject {
                    peer_name,
                    peer,
                    remote_alias,
                } => Some(RemoteProjectInfo {
                    name,
                    peer: peer_name,
                    remote_alias,
                    peer_url: peer.url,
                }),
                _ => None,
            })
            .collect()
    }

    async fn list_projects(&self) -> Result<CallToolResult, McpError> {
        let current_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        let serve_active = self.serve_state.is_some();
        let serve_url = if serve_active {
            Some(serve_url_from_env())
        } else {
            None
        };

        // When serve is active, use ServeState as source of truth for lock status
        if let Some(ref serve_state) = self.serve_state {
            let config = serve_state.config_snapshot();
            let project_groups = config.project_groups();
            let mut repos_info = Vec::new();
            let mut list_warnings: Vec<String> = Vec::new();

            for (alias, path) in &config.repos {
                let db_path = path.join(crate::constants::DB_DIR_NAME);

                let (total_chunks, total_files, model, lock_status, error) = if db_path.exists() {
                    let (model_name, _dims) = read_model_metadata(&db_path);

                    // For repos already opened in DashMap, use the live SharedStores for stats
                    // WITHOUT opening a new VectorStore connection.
                    // For unopened repos, just report metadata — do NOT open the DB.
                    if let Some(stores) = serve_state.get_opened_stores(alias) {
                        let stats_result = {
                            let vs = stores.vector_store.read().await;
                            vs.stats()
                        };
                        // `0 chunks` alone reads exactly like "not indexed yet" — the
                        // repo may in fact be full and simply failing to answer (the
                        // read-only-incident shape this branch exists for). Attribute
                        // the failure to THIS repo rather than a top-level channel:
                        // list_projects returns one entry per repo, so per-item is the
                        // shape that actually matches the fan-out.
                        // `repo_stats_from_result` carries only the part of this
                        // decision that varies by Ok/Err — see its doc comment.
                        // `record_stats_or_warn` wraps it so this call site cannot
                        // silently drop the warning half without also breaking the
                        // counts it returns — see its own doc comment.
                        let (total_chunks, total_files, error) =
                            record_stats_or_warn(stats_result, alias, &mut list_warnings);
                        (
                            total_chunks,
                            total_files,
                            model_name,
                            serve_state
                                .repo_lock_status(alias)
                                .unwrap_or("unknown")
                                .to_string(),
                            error,
                        )
                    } else {
                        // Repo NOT opened — read persisted stats from metadata.json
                        let (md_chunks, md_files) = read_metadata_stats(&db_path);
                        let lock_status = if crate::index::is_database_locked(&db_path) {
                            "locked-externally".to_string()
                        } else {
                            "available".to_string()
                        };
                        (md_chunks, md_files, model_name, lock_status, None)
                    }
                } else {
                    (0, 0, "not indexed".to_string(), "unknown".to_string(), None)
                };

                repos_info.push(RepoInfo {
                    alias: alias.clone(),
                    project_path: path.display().to_string(),
                    database_path: db_path.display().to_string(),
                    total_chunks,
                    total_files,
                    model,
                    lock_status,
                    groups: project_groups.get(alias).cloned().unwrap_or_default(),
                    error,
                });
            }

            let response = ListProjectsResponse {
                repos: repos_info,
                groups: config.groups_with_virtual_all(),
                remote_projects: Self::remote_projects_listing(&config),
                serve_active,
                serve_url,
                current_directory: current_dir.display().to_string(),
            };

            return respond_with_object(&response, &list_warnings);
        }

        // Stdio mode: fall back to disk-based lock detection
        let config = load_repos_config().unwrap_or_default();
        let project_groups = config.project_groups();
        let mut repos_info = Vec::new();
        for (alias, path) in &config.repos {
            let db_path = path.join(crate::constants::DB_DIR_NAME);

            // Get stats
            let (total_chunks, total_files, model, lock_status) = if db_path.exists() {
                let (model_name, dims) = read_model_metadata(&db_path);

                let lock = if crate::index::is_database_locked(&db_path) {
                    "conflicted"
                } else {
                    "available"
                };

                if let Ok(store) = VectorStore::new(&db_path, dims) {
                    if let Ok(stats) = store.stats() {
                        (
                            stats.total_chunks,
                            stats.total_files,
                            model_name,
                            lock.to_string(),
                        )
                    } else {
                        (0, 0, model_name, lock.to_string())
                    }
                } else {
                    (0, 0, model_name, "readonly".to_string())
                }
            } else {
                (0, 0, "not indexed".to_string(), "unknown".to_string())
            };

            // Stdio mode is single-repo-at-a-time CLI usage, not the live multi-repo
            // federation this fan-out fix targets — a stats() failure here is out of
            // scope for this fix (VectorStore::new/stats failing locally is a different
            // shape than a store going down mid-request in a shared serve process).
            repos_info.push(RepoInfo {
                alias: alias.clone(),
                project_path: path.display().to_string(),
                database_path: db_path.display().to_string(),
                total_chunks,
                total_files,
                model,
                lock_status,
                groups: project_groups.get(alias).cloned().unwrap_or_default(),
                error: None,
            });
        }

        let response = ListProjectsResponse {
            repos: repos_info,
            groups: config.groups_with_virtual_all(),
            remote_projects: Self::remote_projects_listing(&config),
            serve_active,
            serve_url,
            current_directory: current_dir.display().to_string(),
        };

        let json = serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}
