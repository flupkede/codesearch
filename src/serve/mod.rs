//! `codesearch serve` — MCP streamable HTTP server mode.
//!
//! Binds on `127.0.0.1:{port}` and serves:
//! - `GET /health` → JSON health check
//! - MCP streamable HTTP at `/mcp` via rmcp tower service
//!
//! Holds a `DashMap<String, Arc<SharedStores>>` keyed by repo alias.
//! Lazy-opens stores on first query. Conflicted repos are isolated.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::response::Json as AxumJson;
use dashmap::DashMap;
use rmcp::transport::{
    StreamableHttpServerConfig, StreamableHttpService,
    streamable_http_server::session::local::LocalSessionManager,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::constants::{
    DEFAULT_SERVE_PORT, HEALTH_PATH, MCP_ENDPOINT_PATH, SERVE_PORT_ENV, DB_DIR_NAME,
};
use crate::db_discovery::repos::ReposConfig;
use crate::index::{IndexManager, SharedStores};
use crate::mcp::types::HealthResponse;

/// Per-repo state managed by the serve instance.
pub(crate) enum RepoState {
    /// Writable repo — full file watching + git HEAD watching active.
    Write {
        stores: Arc<SharedStores>,
        /// Stored for its `Drop` side-effect: dropping the IndexManager stops
        /// the background file watcher thread and releases the write lock.
        #[allow(dead_code)]
        index_manager: Option<Arc<IndexManager>>,
        cancel_token: CancellationToken,
    },
    /// Another process holds the write lock. Read-only access, no live updates.
    Readonly { stores: Arc<SharedStores> },
    /// Both write and readonly open failed.
    Conflicted,
}

impl std::fmt::Debug for RepoState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepoState::Write { .. } => f.debug_struct("RepoState::Write").finish(),
            RepoState::Readonly { .. } => f.debug_struct("RepoState::Readonly").finish(),
            RepoState::Conflicted => f.debug_struct("RepoState::Conflicted").finish(),
        }
    }
}

/// Shared state for the serve mode.
pub(crate) struct ServeState {
    /// Repo alias → opened stores (or conflicted marker).
    repos: DashMap<String, RepoState>,
    /// Loaded repos config (alias → path).
    config: std::sync::RwLock<ReposConfig>,
    /// Last observed mtime of the repos config file.
    config_mtime: std::sync::RwLock<Option<std::time::SystemTime>>,
    /// Optional override for the repos config path (used in tests to avoid env vars).
    config_path_override: Option<PathBuf>,
    /// Test-only counter for reload invocations that actually swapped config.
    #[cfg(test)]
    reload_count: std::sync::atomic::AtomicUsize,
}

impl std::fmt::Debug for ServeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let config = self.config.read().unwrap();
        f.debug_struct("ServeState")
            .field("repo_count", &self.repos.len())
            .field("config_repos", &config.repos.len())
            .finish()
    }
}

impl ServeState {
    fn new(config: ReposConfig, config_path_override: Option<PathBuf>) -> Self {
        Self {
            repos: DashMap::new(),
            config: std::sync::RwLock::new(config),
            config_mtime: std::sync::RwLock::new(None),
            config_path_override,
            #[cfg(test)]
            reload_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Build an actionable conflict error message.
    fn conflicted_msg(alias: &str) -> String {
        format!(
            "Repo '{}' is currently locked by another codesearch process with write access. \
             Stop that process (or let it finish) and retry. If you only need read access, \
             the next query will retry automatically.",
            alias
        )
    }

    /// Reload repos config from disk if the file has changed.
    fn reload_if_changed(&self) -> anyhow::Result<()> {
        let config_path = match self.config_path_override.as_ref() {
            Some(p) => p.clone(),
            None => match ReposConfig::path() {
                Ok(p) => p,
                Err(_) => return Ok(()),
            },
        };

        let mtime = std::fs::metadata(&config_path).and_then(|m| m.modified()).ok();

        let current_mtime = *self.config_mtime.read().map_err(|e| anyhow::anyhow!("Mutex poisoned: {}", e))?;
        if mtime == current_mtime {
            return Ok(()); // no change
        }

        // Load new config; on parse error, keep old config but update mtime to avoid retry storm
        let new_config = match ReposConfig::load_from(&config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to reload repos config: {}. Keeping current config.", e);
                *self.config_mtime.write().map_err(|e| anyhow::anyhow!("Mutex poisoned: {}", e))? = mtime;
                return Ok(());
            }
        };

        // Compute removed aliases under read lock (don't hold it long)
        let removed: Vec<String> = {
            let old = self.config.read().map_err(|e| anyhow::anyhow!("Mutex poisoned: {}", e))?;
            old.repos.keys()
                .filter(|k| !new_config.repos.contains_key(*k))
                .cloned()
                .collect()
        };

        // For each removed alias: fire cancel_token for Write repos, then drop from DashMap.
        // Drop order matters — fire first, remove second, so the spawned FSW task sees
        // cancellation before its RepoState drops.
        for alias in &removed {
            if let Some((_, RepoState::Write { cancel_token, .. })) = self.repos.remove(alias) {
                cancel_token.cancel();
                // Readonly and Conflicted just drop
            }
        }

        // Swap in the new config and mtime.
        // Note: these are two separate writes, so a concurrent reader could observe
        // the new config with the old mtime (or vice versa). This causes at most a
        // spurious extra reload on the next call, which is benign.
        *self.config.write().map_err(|e| anyhow::anyhow!("Mutex poisoned: {}", e))? = new_config;
        *self.config_mtime.write().map_err(|e| anyhow::anyhow!("Mutex poisoned: {}", e))? = mtime;

        #[cfg(test)]
        {
            self.reload_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        Ok(())
    }

    /// Close and remove a repo from the open-stores map.
    ///
    /// Fires the cancel token (stops FSW/git-HEAD watcher) and drops the
    /// RepoState, which releases the LMDB write lock and closes all file handles.
    /// On Windows this is required before deleting the DB directory.
    fn close_repo(&self, alias: &str) {
        if let Some((_, state)) = self.repos.remove(alias) {
            if let RepoState::Write { cancel_token, .. } = state {
                cancel_token.cancel();
            }
            tracing::info!("Closed repo '{}' (released LMDB handles)", alias);
        }
    }

    /// Try to open a repo by alias. Returns a clone of the Arc<SharedStores>
    /// if successful, or an error string if conflicted/unknown.
    pub(crate) async fn get_or_open_stores(
        &self,
        alias: &str,
    ) -> std::result::Result<Arc<SharedStores>, String> {
        let _ = self.reload_if_changed();

        // Fast path: already opened
        if let Some(entry) = self.repos.get(alias) {
            return match entry.value() {
                RepoState::Write { stores, .. } | RepoState::Readonly { stores } => {
                    Ok(stores.clone())
                }
                RepoState::Conflicted => Err(Self::conflicted_msg(alias)),
            };
        }

        // Slow path: need to open
        let path = {
            let config = self.config.read().map_err(|e| format!("Mutex poisoned: {}", e))?;
            config.resolve(alias)
                .ok_or_else(|| format!("Unknown alias '{}'", alias))?
        };

        let db_path = path.join(DB_DIR_NAME);

        // Database existence precheck — don't cache missing DB as Conflicted
        if !db_path.exists() {
            return Err(format!(
                "Database not found at {}. This usually means the repo was removed externally. \
                 Run `codesearch index add {}` to recreate, or `codesearch index rm {}` to clean up the config entry.",
                db_path.display(), path.display(), path.display()
            ));
        }

        // Read dimensions from metadata
        let dims = self.get_dimensions_for_path(&db_path);

        // Try write-mode first, then readonly
        let stores = match SharedStores::new(&db_path, dims) {
            Ok(s) => {
                info!("Opened repo '{}' in write mode", alias);
                s
            }
            Err(_) => {
                // Try readonly
                match SharedStores::new_readonly(&db_path, dims) {
                    Ok(s) => {
                        info!("Opened repo '{}' in readonly mode", alias);
                        let stores_arc = Arc::new(s);
                        self.repos.insert(
                            alias.to_string(),
                            RepoState::Readonly {
                                stores: stores_arc.clone(),
                            },
                        );
                        return Ok(stores_arc);
                    }
                    Err(e) => {
                        warn!("Failed to open repo '{}': {}", alias, e);
                        self.repos
                            .insert(alias.to_string(), RepoState::Conflicted);
                        return Err(Self::conflicted_msg(alias));
                    }
                }
            }
        };

        // Ensure the HNSW vector index is built from existing data.
        // When opening an existing DB, VectorStore starts with indexed=false.
        // Without this, search fails with "Index not built" until the background
        // refresh completes (which may take minutes for large repos).
        {
            let mut vstore = stores.vector_store.write().await;
            match vstore.stats() {
                Ok(s) if s.total_chunks > 0 && !s.indexed => {
                    info!(
                        "Building vector index for '{}' ({} existing chunks)",
                        alias, s.total_chunks
                    );
                    if let Err(e) = vstore.build_index() {
                        warn!("Failed to build vector index for '{}': {}", alias, e);
                    }
                }
                Ok(_) => {} // already indexed or no chunks
                Err(e) => warn!("Could not read stats for '{}': {}", alias, e),
            }
        }

        let stores_arc = Arc::new(stores);

        // Try to create IndexManager for live file watching.
        // On failure, still store as Write — searches keep working, live updates disabled.
        let (index_manager_opt, cancel_token) = {
            let alias_clone = alias.to_string();
            match IndexManager::new_without_refresh(&path, stores_arc.clone()).await {
                Ok(im) => {
                    let im_arc = Arc::new(im);
                    let token = CancellationToken::new();
                    let project_path = path.clone();
                    let db_path_clone = db_path.clone();
                    let stores_for_task = stores_arc.clone();
                    let im_for_task = im_arc.clone();
                    let token_for_task = token.clone();

                    tokio::spawn(async move {
                        // Pre-start FSW so changes during initial refresh aren't lost
                        if let Err(e) = im_for_task.start_watching().await {
                            tracing::warn!(
                                "Could not pre-start FSW for '{}': {}",
                                alias_clone,
                                e
                            );
                        }

                        // Initial incremental refresh
                        if let Err(e) = IndexManager::perform_incremental_refresh_with_stores(
                            &project_path,
                            &db_path_clone,
                            &stores_for_task,
                        )
                        .await
                        {
                            tracing::error!(
                                "Initial refresh for '{}' failed: {}",
                                alias_clone,
                                e
                            );
                        }

                        if token_for_task.is_cancelled() {
                            return;
                        }

                        // Main file watcher loop — runs until cancel_token fires
                        if let Err(e) = im_for_task.start_file_watcher(token_for_task).await {
                            tracing::error!(
                                "File watcher for '{}' stopped: {}",
                                alias_clone,
                                e
                            );
                        }
                    });

                    (Some(im_arc), token)
                }
                Err(e) => {
                    tracing::warn!(
                        "IndexManager init failed for '{}': {} — searches work, live updates disabled",
                        alias_clone,
                        e
                    );
                    let token = CancellationToken::new();
                    token.cancel();
                    (None, token)
                }
            }
        };

        self.repos.insert(
            alias.to_string(),
            RepoState::Write {
                stores: stores_arc.clone(),
                index_manager: index_manager_opt,
                cancel_token,
            },
        );
        Ok(stores_arc)
    }

    fn get_dimensions_for_path(&self, db_path: &std::path::Path) -> usize {
        let metadata_path = db_path.join("metadata.json");
        if let Ok(content) = std::fs::read_to_string(&metadata_path) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(dims) = json.get("dimensions").and_then(|v| v.as_u64()) {
                    return dims as usize;
                }
            }
        }
        crate::constants::DEFAULT_EMBEDDING_DIMENSIONS // default
    }

    /// Get all registered aliases.
    pub(crate) fn aliases(&self) -> Vec<String> {
        let _ = self.reload_if_changed();
        let config = match self.config.read() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Config lock poisoned: {}", e);
                return Vec::new();
            }
        };
        config.repos.keys().cloned().collect()
    }

    /// Get the lock status string for a given alias from the DashMap.
    /// Returns None if the alias is not yet opened (never queried).
    pub(crate) fn repo_lock_status(&self, alias: &str) -> Option<&'static str> {
        match self.repos.get(alias) {
            Some(entry) => match entry.value() {
                RepoState::Write { .. } => Some("write"),
                RepoState::Readonly { .. } => Some("readonly"),
                RepoState::Conflicted => Some("conflicted"),
            },
            None => None,
        }
    }

    /// Get the config (for listing all registered repos and groups).
    /// Triggers reload_if_changed first.
    pub(crate) fn config_snapshot(&self) -> ReposConfig {
        let _ = self.reload_if_changed();
        self.config.read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Resolve a group name to its constituent aliases.
    /// Returns an error if the group doesn't exist.
    pub(crate) fn resolve_group_aliases(&self, group: &str) -> std::result::Result<Vec<String>, String> {
        let _ = self.reload_if_changed();
        let config = match self.config.read() {
            Ok(c) => c,
            Err(e) => return Err(format!("Config lock poisoned: {}", e)),
        };
        config.groups.get(group)
            .cloned()
            .ok_or_else(|| format!("Unknown group '{}'", group))
    }
}

/// Health check handler: GET /health
async fn health_handler() -> AxumJson<serde_json::Value> {
    AxumJson(json!(HealthResponse {
        codesearch_server: true,
        version: env!("CARGO_PKG_VERSION").to_string(),
    }))
}

/// Reindex handler: POST /repos/{alias}/reindex
///
/// Query params:
/// - `force=true` — close the repo, delete the DB, full reindex, reopen.
///   Required when the caller wants a clean rebuild (e.g. `codesearch index -f`).
///   Without force, performs an incremental refresh only.
///
/// Returns 202 Accepted immediately; the reindex runs in the background.
async fn reindex_handler(
    axum::extract::Path(alias): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    axum::extract::State(state): axum::extract::State<Arc<ServeState>>,
) -> (axum::http::StatusCode, axum::response::Json<serde_json::Value>) {
    use axum::http::StatusCode;

    let force = params.get("force").map(|v| v == "true" || v == "1").unwrap_or(false);

    // Resolve the project path for this alias
    let project_path = {
        let config = match state.config.read() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::response::Json(json!({
                        "error": format!("Config lock poisoned: {}", e),
                        "status": "error"
                    })),
                );
            }
        };
        match config.resolve(&alias) {
            Some(p) => p,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    axum::response::Json(json!({
                        "error": format!("Unknown alias '{}'", alias),
                        "status": "not_found"
                    })),
                );
            }
        }
    };

    let db_path = project_path.join(DB_DIR_NAME);
    let alias_bg = alias.clone();

    if force {
        // Force rebuild: close → delete → full reindex → reopen
        // Step 1: close the repo to release LMDB file handles
        state.close_repo(&alias);

        tokio::spawn(async move {
            tracing::info!("🗑️  Force reindex for '{}': closing and deleting DB", alias_bg);

            // Step 2: wait for Windows to release memory-mapped file handles
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            // Step 3: delete the DB directory
            if db_path.exists() {
                if let Err(e) = std::fs::remove_dir_all(&db_path) {
                    tracing::error!(
                        "❌ Force reindex for '{}': failed to delete DB at {}: {}",
                        alias_bg, db_path.display(), e
                    );
                    return;
                }
                tracing::info!("🗑️  Deleted DB at {} for '{}'", db_path.display(), alias_bg);
            }

            // Step 4: full reindex from scratch
            tracing::info!("🔄 Full reindex starting for '{}'", alias_bg);
            match crate::index::index_quiet(Some(project_path), true, CancellationToken::new()).await {
                Ok(()) => {
                    tracing::info!("✅ Force reindex complete for '{}'", alias_bg);
                }
                Err(e) => {
                    tracing::error!("❌ Force reindex failed for '{}': {}", alias_bg, e);
                }
            }
            // Step 5: serve will lazy-reopen on next query (get_or_open_stores)
        });
    } else {
        // Incremental refresh: ensure the repo is opened, then refresh
        let stores = match state.get_or_open_stores(&alias).await {
            Ok(s) => s,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::response::Json(json!({
                        "error": e,
                        "status": "error"
                    })),
                );
            }
        };

        tokio::spawn(async move {
            tracing::info!("🔄 Incremental reindex triggered for '{}' via HTTP API", alias_bg);
            match IndexManager::perform_incremental_refresh_with_stores(
                &project_path,
                &db_path,
                &stores,
            )
            .await
            {
                Ok(()) => {
                    tracing::info!("✅ Reindex complete for '{}'", alias_bg);
                }
                Err(e) => {
                    tracing::error!("❌ Reindex failed for '{}': {}", alias_bg, e);
                }
            }
        });
    }

    (
        StatusCode::ACCEPTED,
        axum::response::Json(json!({
            "status": "accepted",
            "alias": alias,
            "message": "Reindex started in background"
        })),
    )
}

/// Axum middleware: log MCP requests (method + path, skips /health spam).
async fn log_mcp_requests(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    let response = next.run(req).await;

    if path != crate::constants::HEALTH_PATH {
        let status = response.status().as_u16();
        tracing::info!("{} {} → {}", method, path, status);
    }

    response
}

/// Run the MCP serve mode.
///
/// This is the entry point called from CLI when `codesearch serve` is invoked.
pub async fn run_serve(
    port: Option<u16>,
    register_paths: Vec<PathBuf>,
    cancel_token: CancellationToken,
) -> Result<()> {
    let effective_port = port.unwrap_or_else(|| {
        std::env::var(SERVE_PORT_ENV)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_SERVE_PORT)
    });

    // Load repos config (register any --register paths first)
    let mut config = ReposConfig::load().unwrap_or_default();
    for path in &register_paths {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        let alias = config.register(canonical);
        info!("Registered repo '{}' -> {}", alias, path.display());
    }
    if !register_paths.is_empty() {
        config.save().context("Failed to save repos config")?;
    }

    // Auto-discover: if config is empty, scan CWD for a database
    let discovered = config.auto_discover_from_cwd();
    if discovered > 0 {
        if let Err(e) = config.save() {
            warn!("Failed to save auto-discovered repos: {}", e);
        }
    }

    let serve_state = Arc::new(ServeState::new(config, None));

    // Log startup
    let addr = SocketAddr::from(([127, 0, 0, 1], effective_port));
    info!(
        "🚀 Starting codesearch serve v{} on {}",
        env!("CARGO_PKG_VERSION"),
        addr
    );
    info!("📋 Registered repos: {:?}", serve_state.aliases());

    // ── Sequential pre-warming ──
    // Open all registered repos sequentially before accepting connections.
    // This avoids burst I/O and LMDB "already opened with different options"
    // errors when multiple repos are first queried concurrently.
    {
        let aliases = serve_state.aliases();
        if !aliases.is_empty() {
            info!("🔥 Pre-warming {} repos sequentially...", aliases.len());
            for alias in &aliases {
                match serve_state.get_or_open_stores(alias).await {
                    Ok(_) => info!("  ✅ {} ready", alias),
                    Err(e) => warn!("  ⚠️  {} failed: {}", alias, e),
                }
                // Small delay between repos to avoid I/O burst
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            info!("🔥 Pre-warming complete");
        }
    }

    // Create the MCP service factory — each session gets a fresh CodesearchService
    // that uses serve_state for repo routing.
    let state_for_factory = serve_state.clone();
    let session_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let service_factory = move || -> std::result::Result<crate::mcp::CodesearchService, std::io::Error> {
        let session_id = session_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        info!("🔌 MCP client connected (session #{})", session_id);
        // We create a minimal service; actual repo routing is handled inside
        // the tool handlers via serve_state.
        crate::mcp::CodesearchService::new_for_serve(state_for_factory.clone())
            .map_err(std::io::Error::other)
    };

    let session_manager = Arc::new(LocalSessionManager::default());
    let config = StreamableHttpServerConfig::default();

    let mcp_service = StreamableHttpService::new(
        service_factory,
        session_manager,
        config,
    );

    // Build axum router with request logging
    let app = axum::Router::new()
        .route(HEALTH_PATH, axum::routing::get(health_handler))
        .route("/repos/{alias}/reindex", axum::routing::post(reindex_handler))
        .nest_service(MCP_ENDPOINT_PATH, mcp_service)
        .layer(axum::middleware::from_fn(log_mcp_requests))
        .with_state(serve_state.clone());

    // Graceful shutdown
    //
    // axum::serve::with_graceful_shutdown stops accepting new connections when the
    // future resolves, then waits for all existing connections to close before
    // server.await returns. MCP SSE sessions are long-lived and never close on
    // their own, so without a deadline server.await hangs indefinitely after Ctrl-C.
    //
    // Fix: drive server.await in a tokio::select! against a deadline that fires
    // 3 seconds after the cancel_token is cancelled. This gives in-flight HTTP
    // requests time to complete while preventing a permanent hang on open sessions.
    let cancel_for_deadline = cancel_token.clone();
    let server = axum::serve(
        tokio::net::TcpListener::bind(addr).await?,
        app,
    )
    .with_graceful_shutdown(async move {
        cancel_token.cancelled().await;
        info!("🛑 codesearch serve shutting down...");
    });

    info!("✅ codesearch serve ready at http://{}", addr);
    info!("   Health: http://{}{}", addr, HEALTH_PATH);
    info!("   MCP:    http://{}{}", addr, MCP_ENDPOINT_PATH);

    tokio::select! {
        result = server => {
            result.context("Serve error")?;
            info!("✅ codesearch serve shut down cleanly");
        }
        _ = async {
            cancel_for_deadline.cancelled().await;
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        } => {
            // Connections did not drain within 3 s — force-complete shutdown.
            // This is expected when MCP clients hold open SSE sessions.
            info!("⚠️  Shutdown deadline reached — forcing exit (open sessions dropped)");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn state_with_config(config: ReposConfig) -> ServeState {
        // Use a temp file override so reload_if_changed doesn't see the real repos.json
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("repos.json");
        config.save_to(&config_file).unwrap();
        ServeState::new(config, Some(config_file))
    }

    #[tokio::test]
    async fn missing_db_not_cached_as_conflicted() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("myrepo");
        std::fs::create_dir(&repo_path).unwrap();

        let mut config = ReposConfig::default();
        config.register_with_alias(repo_path.clone(), Some("testalias".to_string())).unwrap();

        let state = state_with_config(config);

        // First call: DB missing → error, NOT cached as Conflicted
        let err = match state.get_or_open_stores("testalias").await {
            Err(e) => e,
            Ok(_) => panic!("expected error for missing DB"),
        };
        assert!(err.contains("Database not found"), "expected 'not found', got: {}", err);
        assert!(!state.repos.contains_key("testalias"));

        // Create a minimal DB so next call succeeds
        let db_path = repo_path.join(DB_DIR_NAME);
        std::fs::create_dir(&db_path).unwrap();
        let meta = db_path.join("metadata.json");
        let mut f = std::fs::File::create(&meta).unwrap();
        write!(f, "{{\"dimensions\":384}}").unwrap();
        drop(f);

        // Create the LMDB files (data.mdb and lock.mdb) by opening SharedStores directly
        let _stores = SharedStores::new(&db_path, 384).unwrap();
        drop(_stores);

        // Second call: should succeed without restart
        let res = state.get_or_open_stores("testalias").await;
        assert!(res.is_ok(), "expected ok after recreating DB, got: Err");
    }

    #[tokio::test]
    async fn not_found_error_mentions_fix_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("myrepo");
        std::fs::create_dir(&repo_path).unwrap();

        let mut config = ReposConfig::default();
        config.register_with_alias(repo_path.clone(), Some("testalias".to_string())).unwrap();

        let state = state_with_config(config);
        let err = match state.get_or_open_stores("testalias").await {
            Err(e) => e,
            Ok(_) => panic!("expected error for missing DB"),
        };
        assert!(err.contains("codesearch index add"), "error should mention 'index add': {}", err);
        assert!(err.contains("codesearch index rm"), "error should mention 'index rm': {}", err);
    }

    #[tokio::test]
    async fn conflicted_error_mentions_stop_and_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("myrepo");
        std::fs::create_dir(&repo_path).unwrap();
        let db_path = repo_path.join(DB_DIR_NAME);
        std::fs::create_dir(&db_path).unwrap();
        let meta = db_path.join("metadata.json");
        let mut f = std::fs::File::create(&meta).unwrap();
        write!(f, "{{\"dimensions\":384}}").unwrap();
        drop(f);

        // Open a write lock externally
        let _lock = SharedStores::new(&db_path, 384).unwrap();

        let mut config = ReposConfig::default();
        config.register_with_alias(repo_path.clone(), Some("testalias".to_string())).unwrap();

        let state = state_with_config(config);
        let err = match state.get_or_open_stores("testalias").await {
            Err(e) => e,
            Ok(_) => panic!("expected conflict error"),
        };
        assert!(err.contains("Stop"), "error should mention 'Stop': {}", err);
        assert!(err.contains("retry"), "error should mention 'retry': {}", err);
    }

    #[test]
    fn config_reload_picks_up_new_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("repos.json");

        let repo_a = tmp.path().join("repo-a");
        std::fs::create_dir(&repo_a).unwrap();

        let mut config = ReposConfig::default();
        config.register_with_alias(repo_a.clone(), Some("a".to_string())).unwrap();
        config.save_to(&config_file).unwrap();

        let state = ServeState::new(config, Some(config_file.clone()));
        assert_eq!(state.aliases(), vec!["a"]);

        // Add a new alias directly to the file
        let repo_b = tmp.path().join("repo-b");
        std::fs::create_dir(&repo_b).unwrap();
        let mut config2 = ReposConfig::load_from(&config_file).unwrap();
        config2.register_with_alias(repo_b, Some("b".to_string())).unwrap();

        // Small sleep to ensure mtime changes on Windows
        std::thread::sleep(std::time::Duration::from_millis(150));
        config2.save_to(&config_file).unwrap();

        // Next query should pick it up
        let aliases = state.aliases();
        assert!(aliases.contains(&"a".to_string()));
        assert!(aliases.contains(&"b".to_string()));
    }

    #[tokio::test]
    async fn config_reload_drops_removed_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("repos.json");

        let repo_path = tmp.path().join("myrepo");
        std::fs::create_dir(&repo_path).unwrap();
        let db_path = repo_path.join(DB_DIR_NAME);
        std::fs::create_dir(&db_path).unwrap();
        let meta = db_path.join("metadata.json");
        let mut f = std::fs::File::create(&meta).unwrap();
        write!(f, "{{\"dimensions\":384}}").unwrap();
        drop(f);
        let _stores = SharedStores::new(&db_path, 384).unwrap();
        drop(_stores);

        let mut config = ReposConfig::default();
        config.register_with_alias(repo_path.clone(), Some("x".to_string())).unwrap();
        config.save_to(&config_file).unwrap();

        let state = ServeState::new(config, Some(config_file.clone()));
        // Open alias x so it lands in DashMap
        let _ = state.get_or_open_stores("x").await.unwrap();
        assert!(state.repos.contains_key("x"));

        // Rewrite config without x
        let config2 = ReposConfig::default();

        // Small sleep to ensure mtime changes on Windows
        std::thread::sleep(std::time::Duration::from_millis(150));
        config2.save_to(&config_file).unwrap();

        // Next query for x should fail as unknown
        let err = match state.get_or_open_stores("x").await {
            Err(e) => e,
            Ok(_) => panic!("expected unknown alias after removal"),
        };
        assert!(err.contains("Unknown alias"), "expected unknown alias, got: {}", err);
        assert!(!state.repos.contains_key("x"));
    }

    #[test]
    fn config_reload_no_spurious_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("repos.json");

        let repo_path = tmp.path().join("myrepo");
        std::fs::create_dir(&repo_path).unwrap();

        let mut config = ReposConfig::default();
        config.register_with_alias(repo_path, Some("a".to_string())).unwrap();
        config.save_to(&config_file).unwrap();

        let state = ServeState::new(config, Some(config_file.clone()));
        let initial = state.reload_count.load(std::sync::atomic::Ordering::SeqCst);

        // First call triggers reload (mtime was None)
        let _ = state.aliases();
        let after_first = state.reload_count.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(after_first, initial + 1);

        // Second call without file change should NOT reload
        let _ = state.aliases();
        let after_second = state.reload_count.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(after_second, after_first);
    }

    #[test]
    fn config_reload_tolerates_parse_error() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("repos.json");

        let repo_path = tmp.path().join("myrepo");
        std::fs::create_dir(&repo_path).unwrap();

        let mut config = ReposConfig::default();
        config.register_with_alias(repo_path.clone(), Some("a".to_string())).unwrap();
        config.save_to(&config_file).unwrap();

        let state = ServeState::new(config, Some(config_file.clone()));
        assert!(state.aliases().contains(&"a".to_string()));

        // Overwrite with garbage
        std::fs::write(&config_file, "not-json-at-all").unwrap();

        // Should not panic; old config still usable
        let aliases = state.aliases();
        assert!(aliases.contains(&"a".to_string()));
    }
}
