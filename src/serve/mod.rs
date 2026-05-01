//! `codesearch serve` — MCP streamable HTTP server mode.
//!
//! Binds on `127.0.0.1:{port}` and serves:
//! - `GET /health` → JSON health check
//! - `POST /repos` → register + index + warmup a new repo
//! - `DELETE /repos/:alias` → stop FSW + evict + unregister + delete DB
//! - `POST /repos/:alias/reindex` → trigger incremental or force reindex
//! - MCP streamable HTTP at `/mcp` via rmcp tower service
//!
//! Holds a `DashMap<String, Arc<SharedStores>>` keyed by repo alias.
//! Lazy-opens stores on first query. Conflicted repos are isolated.

mod tui;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::response::Json as AxumJson;
use colored::Colorize;
use dashmap::{DashMap, DashSet};
use rmcp::transport::{
    streamable_http_server::session::local::LocalSessionManager, StreamableHttpServerConfig,
    StreamableHttpService,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::constants::{
    DB_DIR_NAME, DEFAULT_SERVE_PORT, HEALTH_PATH, MCP_ENDPOINT_PATH, REAPER_INTERVAL_SECS,
    REPO_IDLE_TIMEOUT_ENV, REPO_IDLE_TIMEOUT_SECS, SERVE_PORT_ENV,
};
use crate::db_discovery::repos::ReposConfig;
use crate::index::{IndexManager, SharedStores};
use crate::mcp::types::HealthResponse;

/// Lightweight repo status derived from DashMap state only (no DB opens).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepoStateLabel {
    Open,
    Warm,
    Readonly,
    Closed,
    Indexing,
    Error,
    NoIndex,
}

impl RepoStateLabel {
    #[allow(dead_code)]
    fn colored(&self) -> colored::ColoredString {
        match self {
            Self::Open => "Open".green().bold(),
            Self::Warm => "Warm".yellow(),
            Self::Readonly => "Readonly".cyan(),
            Self::Closed => "Closed".dimmed(),
            Self::Indexing => "Indexing".magenta().bold(),
            Self::Error => "Error".red().bold(),
            Self::NoIndex => "No Index".dimmed(),
        }
    }
}

/// Lightweight status info for a single repo (no DB I/O).
pub(crate) struct RepoStatusInfo {
    pub(crate) status: RepoStateLabel,
    pub(crate) changes: u64,
    pub(crate) last_tool_call: Option<String>,
}

/// Format a tool call name and elapsed time into a human-readable string.
fn format_tool_call_ago(tool_name: &str, elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{} ({}s ago)", tool_name, secs)
    } else if secs < 3600 {
        format!("{} ({}m ago)", tool_name, secs / 60)
    } else {
        format!(
            "{} ({}h {}m ago)",
            tool_name,
            secs / 3600,
            (secs % 3600) / 60
        )
    }
}

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
    /// Opened and vector-index built, but NO file system watcher running.
    /// Transitions to `Write` on first actual query (lazy FSW start).
    Warm { stores: Arc<SharedStores> },
    /// Another process holds the write lock. Read-only access, no live updates.
    Readonly { stores: Arc<SharedStores> },
    /// Both write and readonly open failed.
    Conflicted,
}

impl std::fmt::Debug for RepoState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepoState::Write { .. } => f.debug_struct("RepoState::Write").finish(),
            RepoState::Warm { .. } => f.debug_struct("RepoState::Warm").finish(),
            RepoState::Readonly { .. } => f.debug_struct("RepoState::Readonly").finish(),
            RepoState::Conflicted => f.debug_struct("RepoState::Conflicted").finish(),
        }
    }
}

/// Shared state for the serve mode.
pub(crate) struct ServeState {
    /// Repo alias → opened stores (or conflicted marker).
    repos: DashMap<String, RepoState>,
    /// Repo alias → timestamp of last query that touched this repo.
    /// Used by the idle-reaper to evict repos after `REPO_IDLE_TIMEOUT_SECS`.
    last_access: DashMap<String, std::time::Instant>,
    /// Loaded repos config (alias → path).
    config: std::sync::RwLock<ReposConfig>,
    /// Last observed mtime of the repos config file.
    config_mtime: std::sync::RwLock<Option<std::time::SystemTime>>,
    /// Optional override for the repos config path (used in tests to avoid env vars).
    config_path_override: Option<PathBuf>,
    /// Aliases currently being reindexed — prevents concurrent force reindex on the same repo.
    active_reindexes: DashSet<String>,
    /// Per-repo change count since serve started (incremented by index/reindex operations).
    repo_changes: DashMap<String, AtomicU64>,
    /// Per-repo last tool call: (tool_name, timestamp).
    last_tool_call: DashMap<String, (String, std::time::Instant)>,
    /// Currently active MCP sessions.
    active_sessions: AtomicU64,
    /// Total MCP sessions since serve started.
    total_sessions: AtomicU64,
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
            last_access: DashMap::new(),
            config: std::sync::RwLock::new(config),
            config_mtime: std::sync::RwLock::new(None),
            config_path_override,
            active_reindexes: DashSet::new(),
            repo_changes: DashMap::new(),
            last_tool_call: DashMap::new(),
            active_sessions: AtomicU64::new(0),
            total_sessions: AtomicU64::new(0),
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

        let mtime = std::fs::metadata(&config_path)
            .and_then(|m| m.modified())
            .ok();

        let current_mtime = *self
            .config_mtime
            .read()
            .map_err(|e| anyhow::anyhow!("Mutex poisoned: {}", e))?;
        if mtime == current_mtime {
            return Ok(()); // no change
        }

        // Load new config; on parse error, keep old config but update mtime to avoid retry storm
        let new_config = match ReposConfig::load_from(&config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    "Failed to reload repos config: {}. Keeping current config.",
                    e
                );
                *self
                    .config_mtime
                    .write()
                    .map_err(|e| anyhow::anyhow!("Mutex poisoned: {}", e))? = mtime;
                return Ok(());
            }
        };

        // Compute removed aliases under read lock (don't hold it long)
        let removed: Vec<String> = {
            let old = self
                .config
                .read()
                .map_err(|e| anyhow::anyhow!("Mutex poisoned: {}", e))?;
            old.repos
                .keys()
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
            }
            // Warm, Readonly, Conflicted just drop
        }

        // Swap in the new config and mtime.
        // Note: these are two separate writes, so a concurrent reader could observe
        // the new config with the old mtime (or vice versa). This causes at most a
        // spurious extra reload on the next call, which is benign.
        *self
            .config
            .write()
            .map_err(|e| anyhow::anyhow!("Mutex poisoned: {}", e))? = new_config;
        *self
            .config_mtime
            .write()
            .map_err(|e| anyhow::anyhow!("Mutex poisoned: {}", e))? = mtime;

        #[cfg(test)]
        {
            self.reload_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        Ok(())
    }

    /// Stop the file system watcher for a repo by cancelling its token.
    ///
    /// Returns the stores Arc if the repo was open in write or warm mode (so the caller
    /// can still use it for reindexing), or None if the repo wasn't found.
    fn stop_fsw(&self, alias: &str) -> Option<Arc<SharedStores>> {
        if let Some(mut entry) = self.repos.get_mut(alias) {
            match entry.value_mut() {
                RepoState::Write {
                    cancel_token,
                    stores,
                    ..
                } => {
                    cancel_token.cancel();
                    tracing::info!("Stopped FSW for '{}'", alias);
                    return Some(stores.clone());
                }
                RepoState::Warm { stores } => {
                    return Some(stores.clone());
                }
                RepoState::Readonly { stores } => {
                    return Some(stores.clone());
                }
                RepoState::Conflicted => return None,
            }
        }
        None
    }

    /// Spawn the FSW background task for a repo after it has been stopped.
    ///
    /// Creates a fresh IndexManager, performs an initial incremental refresh,
    /// then starts the continuous file watcher loop. Updates the RepoState with
    /// the new cancel token and IndexManager.
    async fn restart_fsw(&self, alias: &str, stores: Arc<SharedStores>) {
        let path = {
            let config = match self.config.read() {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(
                        "Cannot restart FSW for '{}': config lock poisoned: {}",
                        alias,
                        e
                    );
                    return;
                }
            };
            match config.resolve(alias) {
                Some(p) => p,
                None => {
                    tracing::error!("Cannot restart FSW: alias '{}' not in config", alias);
                    return;
                }
            }
        };
        let db_path = path.join(DB_DIR_NAME);

        match IndexManager::new_without_refresh(&path, stores.clone()).await {
            Ok(im) => {
                let im_arc = Arc::new(im);
                let token = CancellationToken::new();
                let alias_bg = alias.to_string();
                let project_path = path.clone();
                let db_path_bg = db_path.clone();
                let stores_bg = stores.clone();
                let im_for_task = im_arc.clone();
                let token_for_task = token.clone();

                tokio::spawn(async move {
                    if let Err(e) = im_for_task.start_watching().await {
                        tracing::warn!("Could not pre-start FSW for '{}': {}", alias_bg, e);
                    }

                    if let Err(e) = IndexManager::perform_incremental_refresh_with_stores(
                        &project_path,
                        &db_path_bg,
                        &stores_bg,
                    )
                    .await
                    {
                        tracing::error!("Post-reindex refresh for '{}' failed: {}", alias_bg, e);
                    }

                    if token_for_task.is_cancelled() {
                        return;
                    }

                    if let Err(e) = im_for_task.start_file_watcher(token_for_task).await {
                        tracing::error!("File watcher for '{}' stopped: {}", alias_bg, e);
                    }
                });

                if let Some(mut entry) = self.repos.get_mut(alias) {
                    *entry.value_mut() = RepoState::Write {
                        stores,
                        index_manager: Some(im_arc),
                        cancel_token: token,
                    };
                }
                tracing::info!("Restarted FSW for '{}'", alias);
            }
            Err(e) => {
                tracing::warn!(
                    "IndexManager init failed for '{}': {} - FSW not restarted, searches still work",
                    alias, e
                );
            }
        }
    }

    /// Warm up a repo by opening its DB, building the vector index, and performing
    /// an incremental refresh — but WITHOUT starting the file system watcher.
    ///
    /// This is used during background pre-warming so the server accepts connections
    /// immediately while repos become search-ready one-by-one. When a repo in `Warm`
    /// state is first queried, `get_or_open_stores()` will transition it to `Write`
    /// and start the FSW lazily.
    pub(crate) async fn warmup_repo(&self, alias: &str) -> std::result::Result<(), String> {
        let _ = self.reload_if_changed();

        // Fast path: already opened in any state
        if let Some(entry) = self.repos.get(alias) {
            match entry.value() {
                RepoState::Write { .. } | RepoState::Warm { .. } | RepoState::Readonly { .. } => {
                    return Ok(());
                }
                RepoState::Conflicted => return Err(Self::conflicted_msg(alias)),
            }
        }

        let path = {
            let config = self
                .config
                .read()
                .map_err(|e| format!("Mutex poisoned: {}", e))?;
            config
                .resolve(alias)
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
                info!("Warmup '{}': opened in write mode", alias);
                s
            }
            Err(_) => match SharedStores::new_readonly(&db_path, dims) {
                Ok(s) => {
                    info!("Warmup '{}': opened in readonly mode", alias);
                    let stores_arc = Arc::new(s);
                    self.repos.insert(
                        alias.to_string(),
                        RepoState::Readonly {
                            stores: stores_arc.clone(),
                        },
                    );
                    return Ok(());
                }
                Err(e) => {
                    warn!("Warmup '{}': failed to open: {}", alias, e);
                    self.repos.insert(alias.to_string(), RepoState::Conflicted);
                    return Err(Self::conflicted_msg(alias));
                }
            },
        };

        // Build vector index from existing data
        {
            let mut vstore = stores.vector_store.write().await;
            match vstore.stats() {
                Ok(s) if s.total_chunks > 0 && !s.indexed => {
                    info!(
                        "Warmup '{}': building vector index ({} existing chunks)",
                        alias, s.total_chunks
                    );
                    if let Err(e) = vstore.build_index() {
                        warn!("Warmup '{}': failed to build vector index: {}", alias, e);
                    }
                }
                Ok(_) => {}
                Err(e) => warn!("Warmup '{}': could not read stats: {}", alias, e),
            }
        }

        let stores_arc = Arc::new(stores);

        // Perform incremental refresh in the background (don't block warmup of next repo)
        let bg_alias = alias.to_string();
        let bg_path = path.clone();
        let bg_db_path = db_path.clone();
        let bg_stores = stores_arc.clone();
        tokio::spawn(async move {
            if let Err(e) = IndexManager::perform_incremental_refresh_with_stores(
                &bg_path,
                &bg_db_path,
                &bg_stores,
            )
            .await
            {
                tracing::warn!("Warmup '{}': incremental refresh failed: {}", bg_alias, e);
            }
        });

        // Store as Warm — FSW will be started lazily on first query.
        // Do NOT touch_access: warmup is background activity, not a real query.
        // The idle timer should only reset when a user/agent actually queries this repo.
        self.repos
            .insert(alias.to_string(), RepoState::Warm { stores: stores_arc });
        Ok(())
    }

    /// Try to open a repo by alias. Returns a clone of the Arc<SharedStores>
    /// if successful, or an error string if conflicted/unknown.
    ///
    /// `touch`: when true, records the access time for idle-eviction tracking.
    /// Pass false for fan-out paths (e.g., multi-repo status, get_chunk candidate
    /// scanning) that should NOT reset the idle timer on every repo.
    pub(crate) async fn get_or_open_stores(
        &self,
        alias: &str,
        touch: bool,
    ) -> std::result::Result<Arc<SharedStores>, String> {
        let _ = self.reload_if_changed();

        // Fast path: already opened
        if let Some(entry) = self.repos.get(alias) {
            if touch {
                self.touch_access(alias);
            }
            return match entry.value() {
                RepoState::Write { stores, .. } | RepoState::Readonly { stores } => {
                    Ok(stores.clone())
                }
                RepoState::Warm { stores } => {
                    // Lazy FSW start: transition Warm → Write only on real query access.
                    // Fan-out/candidate-detection callers pass touch=false and must not
                    // trigger Warm → Write or start FSW.
                    let stores = stores.clone();
                    if !touch {
                        return Ok(stores);
                    }
                    drop(entry); // release DashMap read guard before mutation

                    // Only one caller should do the transition; use a compare-and-swap pattern.
                    // Check if someone else already transitioned it.
                    if let Some(mut mut_entry) = self.repos.get_mut(alias) {
                        if let RepoState::Write { stores, .. } = mut_entry.value() {
                            return Ok(stores.clone());
                        }
                        if let RepoState::Warm { stores } = mut_entry.value() {
                            let stores = stores.clone();
                            let path = {
                                let config = self
                                    .config
                                    .read()
                                    .map_err(|e| format!("Mutex poisoned: {}", e))?;
                                config
                                    .resolve(alias)
                                    .ok_or_else(|| format!("Unknown alias '{}'", alias))?
                            };

                            // Start FSW in background for this repo
                            self.spawn_fsw_for_warm(alias, &path, stores.clone(), &mut mut_entry);
                            return Ok(stores);
                        }
                        // Someone else transitioned it already
                        if let RepoState::Readonly { stores } = mut_entry.value() {
                            return Ok(stores.clone());
                        }
                        if let RepoState::Conflicted = mut_entry.value() {
                            return Err(Self::conflicted_msg(alias));
                        }
                    }
                    Ok(stores)
                }
                RepoState::Conflicted => Err(Self::conflicted_msg(alias)),
            };
        }

        // Slow path: need to open
        let path = {
            let config = self
                .config
                .read()
                .map_err(|e| format!("Mutex poisoned: {}", e))?;
            config
                .resolve(alias)
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
                        if touch {
                            self.touch_access(alias);
                        }
                        return Ok(stores_arc);
                    }
                    Err(e) => {
                        warn!("Failed to open repo '{}': {}", alias, e);
                        self.repos.insert(alias.to_string(), RepoState::Conflicted);
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
                            tracing::warn!("Could not pre-start FSW for '{}': {}", alias_clone, e);
                        }

                        // Initial incremental refresh
                        if let Err(e) = IndexManager::perform_incremental_refresh_with_stores(
                            &project_path,
                            &db_path_clone,
                            &stores_for_task,
                        )
                        .await
                        {
                            tracing::error!("Initial refresh for '{}' failed: {}", alias_clone, e);
                        }

                        if token_for_task.is_cancelled() {
                            return;
                        }

                        // Main file watcher loop — runs until cancel_token fires
                        if let Err(e) = im_for_task.start_file_watcher(token_for_task).await {
                            tracing::error!("File watcher for '{}' stopped: {}", alias_clone, e);
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
        if touch {
            self.touch_access(alias);
        }
        Ok(stores_arc)
    }

    /// Spawn the file system watcher for a repo that was warmed up without FSW.
    ///
    /// Called from `get_or_open_stores()` when a `Warm` repo receives its first
    /// actual query. Transitions `Warm` → `Write` with a live FSW.
    fn spawn_fsw_for_warm(
        &self,
        alias: &str,
        project_path: &std::path::Path,
        stores: Arc<SharedStores>,
        entry: &mut dashmap::mapref::one::RefMut<String, RepoState>,
    ) {
        let alias_bg = alias.to_string();
        let path_bg = project_path.to_path_buf();
        let stores_bg = stores.clone();

        let cancel_token = CancellationToken::new();
        let token_for_task = cancel_token.clone();

        // Fire-and-forget: create IndexManager + start FSW in background.
        // We don't block the first query — the repo is already searchable from the Warm state.
        tokio::spawn(async move {
            if token_for_task.is_cancelled() {
                return;
            }

            match IndexManager::new_without_refresh(&path_bg, stores_bg.clone()).await {
                Ok(im) => {
                    let im_arc = Arc::new(im);
                    let im_for_task = im_arc.clone();

                    if token_for_task.is_cancelled() {
                        return;
                    }

                    if let Err(e) = im_for_task.start_watching().await {
                        tracing::warn!(
                            "Lazy FSW start for '{}': pre-start failed: {}",
                            alias_bg,
                            e
                        );
                    }

                    if token_for_task.is_cancelled() {
                        return;
                    }

                    if let Err(e) = im_for_task.start_file_watcher(token_for_task).await {
                        tracing::error!("Lazy FSW for '{}' stopped: {}", alias_bg, e);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Lazy FSW for '{}': IndexManager init failed: {} — live updates disabled",
                        alias_bg,
                        e
                    );
                }
            }
        });

        // Transition to Write immediately so future requests see this repo as active.
        // The IndexManager is created inside the spawned task, so we store None here.
        // The cancel_token is the real token used by that task and can stop FSW via stop_fsw().
        *entry.value_mut() = RepoState::Write {
            stores,
            index_manager: None,
            cancel_token,
        };
        tracing::info!("Lazy FSW started for '{}' (Warm → Write)", alias);
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
                RepoState::Warm { .. } => Some("warm"),
                RepoState::Readonly { .. } => Some("readonly"),
                RepoState::Conflicted => Some("conflicted"),
            },
            None => None,
        }
    }

    /// Get the SharedStores for an already-opened repo (no DB open).
    /// Returns None if the repo is not opened or is in Conflicted state.
    pub(crate) fn get_opened_stores(&self, alias: &str) -> Option<Arc<SharedStores>> {
        self.repos.get(alias).and_then(|entry| match entry.value() {
            RepoState::Write { stores, .. } => Some(stores.clone()),
            RepoState::Warm { stores } => Some(stores.clone()),
            RepoState::Readonly { stores } => Some(stores.clone()),
            RepoState::Conflicted => None,
        })
    }

    /// Get the config (for listing all registered repos and groups).
    /// Triggers reload_if_changed first.
    pub(crate) fn config_snapshot(&self) -> ReposConfig {
        let _ = self.reload_if_changed();
        self.config
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Resolve a group name to its constituent aliases.
    /// Returns an error if the group doesn't exist.
    pub(crate) fn resolve_group_aliases(
        &self,
        group: &str,
    ) -> std::result::Result<Vec<String>, String> {
        let _ = self.reload_if_changed();
        let config = match self.config.read() {
            Ok(c) => c,
            Err(e) => return Err(format!("Config lock poisoned: {}", e)),
        };
        config
            .groups
            .get(group)
            .cloned()
            .ok_or_else(|| format!("Unknown group '{}'", group))
    }

    /// Record that a repo was just accessed (query or reindex).
    /// Called from `get_or_open_stores(touch=true)`, and `reindex_handler`.
    /// NOT called from `warmup_repo` — background warmup is not a real query.
    pub(crate) fn touch_access(&self, alias: &str) {
        self.last_access
            .insert(alias.to_string(), std::time::Instant::now());
    }

    /// Record a tool call for a specific repo (for dashboard display).
    pub(crate) fn record_tool_call(&self, alias: &str, tool_name: &str) {
        self.last_tool_call.insert(
            alias.to_string(),
            (tool_name.to_string(), std::time::Instant::now()),
        );
    }

    /// Record that changes were made to a repo (index/reindex).
    #[allow(dead_code)]
    pub(crate) fn record_changes(&self, alias: &str, count: u64) {
        self.repo_changes
            .entry(alias.to_string())
            .and_modify(|c| {
                c.fetch_add(count, Ordering::Relaxed);
            })
            .or_insert_with(|| AtomicU64::new(count));
    }

    /// Increment active session count. Returns the new session ID.
    pub(crate) fn session_connected(&self) -> u64 {
        self.active_sessions.fetch_add(1, Ordering::Relaxed);
        self.total_sessions.fetch_add(1, Ordering::Relaxed)
    }

    /// Decrement active session count.
    pub(crate) fn session_disconnected(&self) {
        self.active_sessions.fetch_sub(1, Ordering::Relaxed);
    }

    /// Get the current number of active sessions.
    #[allow(dead_code)]
    pub(crate) fn active_session_count(&self) -> u64 {
        self.active_sessions.load(Ordering::Relaxed)
    }

    /// Get lightweight repo statuses WITHOUT opening any databases.
    /// Returns a list of (alias, status_info) where status is derived from DashMap state only.
    pub(crate) fn repo_statuses_lightweight(&self) -> Vec<(String, RepoStatusInfo)> {
        let config = match self.config.read() {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };

        let mut result = Vec::with_capacity(config.repos.len());
        for (alias, path) in &config.repos {
            let db_path = path.join(DB_DIR_NAME);
            let db_exists = db_path.exists();

            let label = if self.active_reindexes.contains(alias) {
                RepoStateLabel::Indexing
            } else {
                match self.repos.get(alias) {
                    Some(entry) => match entry.value() {
                        RepoState::Write { .. } => RepoStateLabel::Open,
                        RepoState::Warm { .. } => RepoStateLabel::Warm,
                        RepoState::Readonly { .. } => RepoStateLabel::Readonly,
                        RepoState::Conflicted => RepoStateLabel::Error,
                    },
                    None => {
                        if !db_exists {
                            RepoStateLabel::NoIndex
                        } else {
                            RepoStateLabel::Closed
                        }
                    }
                }
            };

            let changes = match self.repos.get(alias) {
                Some(entry) => match entry.value() {
                    RepoState::Write { stores, .. }
                    | RepoState::Warm { stores }
                    | RepoState::Readonly { stores } => {
                        stores.changes_count.load(Ordering::Relaxed)
                    }
                    RepoState::Conflicted => 0,
                },
                None => self
                    .repo_changes
                    .get(alias)
                    .map(|c| c.load(Ordering::Relaxed))
                    .unwrap_or(0),
            };

            let last_tool = self
                .last_tool_call
                .get(alias)
                .map(|e| (e.value().0.clone(), e.value().1.elapsed()))
                .map(|(name, ago)| format_tool_call_ago(&name, ago));

            result.push((
                alias.clone(),
                RepoStatusInfo {
                    status: label,
                    changes,
                    last_tool_call: last_tool,
                },
            ));
        }
        result
    }

    /// Print a formatted dashboard table to stderr.
    /// Only used for debugging; the TUI replaces this in production.
    #[allow(dead_code)]
    pub(crate) fn print_dashboard(&self) {
        let repos = self.repo_statuses_lightweight();
        if repos.is_empty() {
            return;
        }

        let active = self.active_sessions.load(Ordering::Relaxed);
        let total = self.total_sessions.load(Ordering::Relaxed);

        // Column widths (min 10 for status to fit "Readonly")
        let alias_w = repos.iter().map(|(a, _)| a.len()).max().unwrap_or(5).max(5);
        let status_w = 10;

        let sep = "─".repeat(alias_w + 2);
        let sep_s = "─".repeat(status_w + 2);
        let sep_c = "─".repeat(9);
        let sep_t = "─".repeat(26);

        let top = format!(
            "{}{}{}{}{}{}{}{}{}",
            "╭", sep, "┬", sep_s, "┬", sep_c, "┬", sep_t, "╮"
        );
        let mid = format!(
            "{}{}{}{}{}{}{}{}{}",
            "╞", sep, "╪", sep_s, "╪", sep_c, "╪", sep_t, "╡"
        );
        let bot = format!(
            "{}{}{}{}{}{}{}{}{}",
            "╰", sep, "┴", sep_s, "┴", sep_c, "┴", sep_t, "╯"
        );

        eprintln!();
        eprintln!("{}", top.bright_black());

        // Header
        eprintln!(
            "{} {:<w_alias$} {} {:<w_status$} {} {:>7} {} {:<24} {}",
            "│".bright_black(),
            "Project".bold(),
            "│".bright_black(),
            "Status".bold(),
            "│".bright_black(),
            "Changes".bold(),
            "│".bright_black(),
            "Last Tool Call".bold(),
            "│".bright_black(),
            w_alias = alias_w,
            w_status = status_w,
        );

        eprintln!("{}", mid.bright_black());

        // Rows
        for (alias, info) in &repos {
            // Format status as plain text first, then apply color.
            // This avoids ANSI escape codes interfering with padding alignment.
            let status_plain = match info.status {
                RepoStateLabel::Open => "Open",
                RepoStateLabel::Warm => "Warm",
                RepoStateLabel::Readonly => "Readonly",
                RepoStateLabel::Closed => "Closed",
                RepoStateLabel::Indexing => "Indexing",
                RepoStateLabel::Error => "Error",
                RepoStateLabel::NoIndex => "No Index",
            };
            let status_colored = info.status.colored();
            let status_padded = format!("{:<w_status$}", status_plain, w_status = status_w);
            // Replace the plain text with the colored version
            let status_display = status_padded.replace(status_plain, &status_colored.to_string());
            let tool_str = info.last_tool_call.as_deref().unwrap_or("—");
            eprintln!(
                "{} {:<w_alias$} {} {} {} {:>7} {} {:<24} {}",
                "│".bright_black(),
                alias,
                "│".bright_black(),
                status_display,
                "│".bright_black(),
                info.changes,
                "│".bright_black(),
                tool_str,
                "│".bright_black(),
                w_alias = alias_w,
            );
        }

        eprintln!("{}", bot.bright_black());

        // Overall status
        let has_error = repos
            .iter()
            .any(|(_, r)| matches!(r.status, RepoStateLabel::Error));
        let health = if has_error {
            "Error".red().bold().to_string()
        } else {
            "Healthy".green().bold().to_string()
        };

        let open_count = repos
            .iter()
            .filter(|(_, r)| matches!(r.status, RepoStateLabel::Open))
            .count();
        let warm_count = repos
            .iter()
            .filter(|(_, r)| matches!(r.status, RepoStateLabel::Warm))
            .count();
        let closed_count = repos
            .iter()
            .filter(|(_, r)| matches!(r.status, RepoStateLabel::Closed | RepoStateLabel::NoIndex))
            .count();

        eprintln!();
        eprintln!(
            "  {} {}   {} {}   {} {}   {} {}",
            "Status:".dimmed(),
            health,
            "Open:".dimmed(),
            format!("{}", open_count).green(),
            "Warm:".dimmed(),
            format!("{}", warm_count).yellow(),
            "Closed:".dimmed(),
            format!("{}", closed_count).dimmed(),
        );
        eprintln!(
            "  {} {}   {} {}",
            "Active Sessions:".dimmed(),
            format!("{}", active).cyan(),
            "Total Since Start:".dimmed(),
            format!("{}", total).dimmed(),
        );
        eprintln!();
    }

    /// Get the configured idle timeout duration.
    /// Reads from env var if set, falls back to the compile-time constant.
    fn idle_timeout(&self) -> std::time::Duration {
        std::env::var(REPO_IDLE_TIMEOUT_ENV)
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&s| s > 0)
            .map(std::time::Duration::from_secs)
            .unwrap_or_else(|| std::time::Duration::from_secs(REPO_IDLE_TIMEOUT_SECS))
    }

    /// Evict all repos that have been idle longer than the timeout.
    ///
    /// Closes DB handles, stops FSW, and releases memory. The repo will be
    /// automatically re-opened (and re-warmed) on the next query.
    /// Active reindexes are never evicted.
    pub(crate) fn evict_idle_repos(&self) {
        let timeout = self.idle_timeout();
        let now = std::time::Instant::now();

        // Collect aliases to evict (can't mutate DashMap while iterating)
        let to_evict: Vec<String> = self
            .last_access
            .iter()
            .filter(|entry| {
                let alias = entry.key();
                // Don't evict repos that are being reindexed
                if self.active_reindexes.contains(alias) {
                    return false;
                }
                now.duration_since(*entry.value()) >= timeout
            })
            .map(|entry| entry.key().clone())
            .collect();

        // Log reaper status even when nothing to evict (for debugging idle eviction)
        if !self.last_access.is_empty() {
            let idle_ages: Vec<(String, u64)> = self
                .last_access
                .iter()
                .map(|e| (e.key().clone(), now.duration_since(*e.value()).as_secs()))
                .collect();
            tracing::debug!(
                "🔍 Reaper check: {} repos tracked, {} eligible for eviction (timeout={}m). Ages: {:?}",
                self.last_access.len(),
                to_evict.len(),
                timeout.as_secs() / 60,
                idle_ages,
            );
        }

        if to_evict.is_empty() {
            return;
        }

        for alias in &to_evict {
            match self.repos.remove(alias) {
                Some((_, RepoState::Write { cancel_token, .. })) => {
                    cancel_token.cancel();
                    self.last_access.remove(alias);
                    info!("🕐 Evicted idle repo '{}' (FSW stopped, DB closed)", alias);
                }
                Some((_, RepoState::Warm { .. } | RepoState::Readonly { .. })) => {
                    self.last_access.remove(alias);
                    info!("🕐 Evicted idle repo '{}' (DB closed)", alias);
                }
                Some((_, RepoState::Conflicted)) => {
                    self.last_access.remove(alias);
                }
                None => {
                    self.last_access.remove(alias);
                }
            }
        }

        if !to_evict.is_empty() {
            info!(
                "🕐 Idle reaper: evicted {} repo(s), {} still open",
                to_evict.len(),
                self.repos.len()
            );
        }
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
) -> (
    axum::http::StatusCode,
    axum::response::Json<serde_json::Value>,
) {
    use axum::http::StatusCode;

    let force = params
        .get("force")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);

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

    // Concurrent reindex guard — reject if this alias is already being reindexed
    if !state.active_reindexes.insert(alias_bg.clone()) {
        return (
            StatusCode::CONFLICT,
            axum::response::Json(json!({
                "error": format!("Reindex already in progress for '{}'", alias),
                "status": "conflict"
            })),
        );
    }

    // Ensure the guard is removed when we return early or the background task finishes.
    let guard_alias = alias_bg.clone();
    let guard_state = state.clone();

    if force {
        // Force rebuild: stop FSW -> clear data in-place -> full reindex -> restart FSW.
        // The FSW must be stopped before clearing the FileMetaStore, otherwise it
        // sees all the file writes during reindex as "new changes" and triggers
        // endless incremental refresh cycles.

        // 1. Stop the FSW (cancel its token)
        let stores = match state.stop_fsw(&alias) {
            Some(s) => s,
            None => {
                // FSW not running -- try opening normally
                match state.get_or_open_stores(&alias, true).await {
                    Ok(s) => s,
                    Err(e) => {
                        state.active_reindexes.remove(&guard_alias);
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            axum::response::Json(json!({
                                "error": e,
                                "status": "error"
                            })),
                        );
                    }
                }
            }
        };

        let g_alias = guard_alias.clone();
        let g_state = guard_state.clone();
        tokio::spawn(async move {
            tracing::info!(
                "Force reindex for '{}': clearing stores and reindexing",
                alias_bg
            );

            // 2. Clear data and reindex
            match IndexManager::force_reindex_with_stores(&project_path, &db_path, &stores).await {
                Ok(()) => {
                    tracing::info!("Force reindex complete for '{}'", alias_bg);
                }
                Err(e) => {
                    tracing::error!("Force reindex failed for '{}': {}", alias_bg, e);
                }
            }

            // 3. Restart FSW with fresh IndexManager
            g_state.restart_fsw(&g_alias, stores).await;

            g_state.active_reindexes.remove(&g_alias);
        });
    } else {
        // Incremental refresh: ensure the repo is opened, then refresh
        let stores = match state.get_or_open_stores(&alias, true).await {
            Ok(s) => s,
            Err(e) => {
                state.active_reindexes.remove(&guard_alias);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::response::Json(json!({
                        "error": e,
                        "status": "error"
                    })),
                );
            }
        };

        let g_alias = guard_alias.clone();
        let g_state = guard_state.clone();
        tokio::spawn(async move {
            tracing::info!(
                "🔄 Incremental reindex triggered for '{}' via HTTP API",
                alias_bg
            );
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
            g_state.active_reindexes.remove(&g_alias);
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

/// Request body for POST /repos
#[derive(serde::Deserialize)]
struct AddRepoRequest {
    /// Absolute or relative path to the project directory (required).
    path: PathBuf,
    /// Optional alias to register under. If omitted, the directory name is used.
    alias: Option<String>,
    /// Create a global index instead of local.
    #[serde(default)]
    global: bool,
}

/// Add-repo handler: POST /repos
///
/// Registers a new repo in repos.json, creates the index, and warms it up.
/// Returns 201 on success.
async fn add_repo_handler(
    axum::extract::State(state): axum::extract::State<Arc<ServeState>>,
    axum::extract::Json(body): axum::extract::Json<AddRepoRequest>,
) -> (
    axum::http::StatusCode,
    axum::response::Json<serde_json::Value>,
) {
    use axum::http::StatusCode;

    // Canonicalize the path
    let canonical_path = match body.path.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::response::Json(json!({
                    "error": format!("Cannot canonicalize path '{}': {}", body.path.display(), e),
                    "status": "error"
                })),
            );
        }
    };

    // Register in repos.json
    let alias = {
        let mut config = match state.config.write() {
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

        // Check if already registered
        if let Some(existing_alias) = config.alias_for_path(&canonical_path) {
            return (
                StatusCode::CONFLICT,
                axum::response::Json(json!({
                    "error": format!("Path already registered as '{}'", existing_alias),
                    "status": "conflict",
                    "alias": existing_alias,
                })),
            );
        }

        let alias = match config.register_with_alias(canonical_path.clone(), body.alias.clone()) {
            Ok(a) => a,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::response::Json(json!({
                        "error": format!("Registration failed: {}", e),
                        "status": "error"
                    })),
                );
            }
        };

        if let Err(e) = config.save() {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::response::Json(json!({
                    "error": format!("Failed to save repos config: {}", e),
                    "status": "error"
                })),
            );
        }

        alias
    };

    // Create the index using index_quiet
    let cancel_token = CancellationToken::new();
    let index_path = canonical_path.clone();
    let alias_bg = alias.clone();
    let state_bg = state.clone();

    match crate::index::index_quiet(Some(index_path.clone()), false, body.global, cancel_token)
        .await
    {
        Ok(()) => {
            tracing::info!("Index created for '{}' ({})", alias, index_path.display());
        }
        Err(e) => {
            // Index failed — remove the config entry we just added
            tracing::error!("Index creation failed for '{}': {}", alias, e);
            {
                if let Ok(mut config) = state.config.write() {
                    config.unregister_alias(&alias);
                    let _ = config.save();
                }
            }
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::response::Json(json!({
                    "error": format!("Index creation failed: {}", e),
                    "status": "error"
                })),
            );
        }
    }

    // Warmup the repo (opens DB, builds vector index, stores as Warm)
    tokio::spawn(async move {
        if let Err(e) = state_bg.warmup_repo(&alias_bg).await {
            tracing::warn!("Warmup failed for newly added repo '{}': {}", alias_bg, e);
        }
    });

    (
        StatusCode::CREATED,
        axum::response::Json(json!({
            "status": "created",
            "alias": alias,
            "path": canonical_path,
            "message": "Repo registered, indexed, and warming up"
        })),
    )
}

/// Remove-repo handler: DELETE /repos/:alias
///
/// Stops the FSW, evicts the repo from memory, unregisters from repos.json,
/// and deletes the database directory. Returns 200 on success.
async fn remove_repo_handler(
    axum::extract::Path(alias): axum::extract::Path<String>,
    axum::extract::State(state): axum::extract::State<Arc<ServeState>>,
) -> (
    axum::http::StatusCode,
    axum::response::Json<serde_json::Value>,
) {
    use axum::http::StatusCode;

    // 1. Resolve project path from config
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

    // 2. Stop FSW and evict from memory.
    // Drop the returned stores Arc explicitly so DB handles are released ASAP.
    {
        let stores = state.stop_fsw(&alias);
        drop(stores);
    }
    state.repos.remove(&alias);
    state.last_access.remove(&alias);
    tracing::info!("Evicted repo '{}' from memory", alias);

    // 3. Unregister from repos.json
    {
        let mut config = match state.config.write() {
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
        config.unregister_alias(&alias);
        if let Err(e) = config.save() {
            tracing::warn!(
                "Failed to save repos config after removing '{}': {}",
                alias,
                e
            );
        }
    }

    // 4. Delete the database directory.
    //    Background tasks (incremental refresh) may still hold Arc<SharedStores>
    //    clones for a brief moment after eviction. Retry with a short delay so
    //    those tasks finish and release their file handles — critical on Windows
    //    where open file handles block directory deletion (os error 32).
    if db_path.exists() {
        let mut deleted = false;
        for attempt in 0..5 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
            match std::fs::remove_dir_all(&db_path) {
                Ok(()) => {
                    tracing::info!("Deleted database for '{}': {}", alias, db_path.display());
                    deleted = true;
                    break;
                }
                Err(e) if attempt < 4 => {
                    tracing::debug!(
                        "DB delete attempt {} for '{}' failed (will retry): {}",
                        attempt + 1,
                        alias,
                        e
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to delete database for '{}' after 5 attempts (may be locked): {}",
                        alias,
                        e
                    );
                }
            }
        }
        let _ = deleted; // used for logging only
    }

    (
        StatusCode::OK,
        axum::response::Json(json!({
            "status": "removed",
            "alias": alias,
            "path": project_path,
            "message": "Repo removed: FSW stopped, evicted from memory, unregistered, DB deleted"
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
        eprintln!("Registered repo '{}' -> {}", alias, path.display());
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
    eprintln!(
        "🚀 Starting codesearch serve v{} on {}",
        env!("CARGO_PKG_VERSION"),
        addr
    );
    let repo_list = format!("{:?}", serve_state.aliases());
    info!("📋 Registered repos: {}", repo_list);
    eprintln!("📋 Registered repos: {}", repo_list);

    // ── Start HTTP server FIRST ──
    // Accept connections immediately so MCP clients don't time out.
    // Pre-warming runs in the background below.

    // Create the MCP service factory — each session gets a fresh CodesearchService
    // that uses serve_state for repo routing.
    let state_for_factory = serve_state.clone();
    let service_factory =
        move || -> std::result::Result<crate::mcp::CodesearchService, std::io::Error> {
            let session_id = state_for_factory.session_connected();
            info!("🔌 MCP client connected (session #{})", session_id);
            // We create a minimal service; actual repo routing is handled inside
            // the tool handlers via serve_state.
            crate::mcp::CodesearchService::new_for_serve(state_for_factory.clone())
                .map_err(std::io::Error::other)
        };

    // Build session manager with extended keep_alive (default is 5 min which kills
    // idle MCP sessions too aggressively). 30 minutes matches our repo idle eviction.
    let mut session_manager = LocalSessionManager::default();
    session_manager.session_config.keep_alive = Some(std::time::Duration::from_secs(30 * 60));
    let session_manager = Arc::new(session_manager);
    let config = StreamableHttpServerConfig::default();

    let mcp_service = StreamableHttpService::new(service_factory, session_manager, config);

    // Build axum router with request logging.
    // Stale-session recovery is handled client-side by the stdio proxy's retry
    // loop in `McpProxyService` (see src/mcp/mod.rs). Remote MCP clients that
    // are not spec-compliant must reconnect themselves — we do not attempt a
    // server-side transparent reconnect because that path opened a session leak
    // and could not actually reach OpenCode (TCP keep-alive failure happens
    // before the request hits this middleware).
    let app = axum::Router::new()
        .route(HEALTH_PATH, axum::routing::get(health_handler))
        .route("/repos", axum::routing::post(add_repo_handler))
        .route("/repos/:alias", axum::routing::delete(remove_repo_handler))
        .route(
            "/repos/:alias/reindex",
            axum::routing::post(reindex_handler),
        )
        .nest_service(MCP_ENDPOINT_PATH, mcp_service)
        .layer(axum::middleware::from_fn(log_mcp_requests))
        .with_state(serve_state.clone());

    // Bind TCP listener BEFORE spawning background warmup, so we know the port is live.
    let listener = tokio::net::TcpListener::bind(addr).await?;

    info!("✅ codesearch serve ready at http://{}", addr);
    info!("   Health: http://{}{}", addr, HEALTH_PATH);
    info!("   MCP:    http://{}{}", addr, MCP_ENDPOINT_PATH);

    // ── Start TUI (if TTY available) ──
    // When a real terminal is attached, launch the fullscreen ratatui TUI.
    // When piped / no TTY, fall back to periodic eprintln dashboard.
    let serve_url = format!("http://{}", addr);
    let tui_cancel = cancel_token.clone();
    let tui_state = serve_state.clone();
    let tui_url = serve_url.clone();

    let tui_handle = tui::maybe_spawn_tui(tui_state, tui_cancel, tui_url);

    // ── Background pre-warming (NO FSW) ──
    // Open all registered repos sequentially: opens DB, builds vector index,
    // starts incremental refresh — but does NOT start file system watchers.
    // FSW is started lazily on first query via get_or_open_stores().
    // This saves memory and overhead for repos that are never queried.
    {
        let warmup_state = serve_state.clone();
        tokio::spawn(async move {
            let aliases = warmup_state.aliases();
            if !aliases.is_empty() {
                info!("🔥 Background warming {} repos (no FSW)...", aliases.len());
                for alias in &aliases {
                    match warmup_state.warmup_repo(alias).await {
                        Ok(()) => info!("  ✅ {} warmed (no FSW)", alias),
                        Err(e) => warn!("  ⚠️  {} warmup failed: {}", alias, e),
                    }
                    // Small delay between repos to avoid I/O burst
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                info!("🔥 Background warming complete");
            }
        });
    }

    // ── Idle reaper ──
    // Periodically evicts repos that haven't been queried for REPO_IDLE_TIMEOUT_SECS.
    // Stops FSW, closes DB handles, releases memory. Re-opens on next query.
    {
        let reaper_state = serve_state.clone();
        let reaper_cancel = cancel_token.clone();
        tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(REAPER_INTERVAL_SECS);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {
                        reaper_state.evict_idle_repos();
                        // Dashboard refresh handled by TUI auto-refresh (TTY) or not needed (non-TTY)
                    }
                    _ = reaper_cancel.cancelled() => {
                        break;
                    }
                }
            }
        });
    }

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
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        cancel_token.cancelled().await;
        info!("🛑 codesearch serve shutting down...");
    });

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

    // Wait for the TUI task to finish cleanup (restore terminal).
    // The TUI's Drop guard restores the terminal, so we need to give it
    // a moment before the process exits.
    if let Some(handle) = tui_handle {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), handle).await;
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
        config
            .register_with_alias(repo_path.clone(), Some("testalias".to_string()))
            .unwrap();

        let state = state_with_config(config);

        // First call: DB missing → error, NOT cached as Conflicted
        let err = match state.get_or_open_stores("testalias", true).await {
            Err(e) => e,
            Ok(_) => panic!("expected error for missing DB"),
        };
        assert!(
            err.contains("Database not found"),
            "expected 'not found', got: {}",
            err
        );
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
        let res = state.get_or_open_stores("testalias", true).await;
        assert!(res.is_ok(), "expected ok after recreating DB, got: Err");
    }

    #[tokio::test]
    async fn not_found_error_mentions_fix_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("myrepo");
        std::fs::create_dir(&repo_path).unwrap();

        let mut config = ReposConfig::default();
        config
            .register_with_alias(repo_path.clone(), Some("testalias".to_string()))
            .unwrap();

        let state = state_with_config(config);
        let err = match state.get_or_open_stores("testalias", true).await {
            Err(e) => e,
            Ok(_) => panic!("expected error for missing DB"),
        };
        assert!(
            err.contains("codesearch index add"),
            "error should mention 'index add': {}",
            err
        );
        assert!(
            err.contains("codesearch index rm"),
            "error should mention 'index rm': {}",
            err
        );
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
        config
            .register_with_alias(repo_path.clone(), Some("testalias".to_string()))
            .unwrap();

        let state = state_with_config(config);
        let err = match state.get_or_open_stores("testalias", true).await {
            Err(e) => e,
            Ok(_) => panic!("expected conflict error"),
        };
        assert!(err.contains("Stop"), "error should mention 'Stop': {}", err);
        assert!(
            err.contains("retry"),
            "error should mention 'retry': {}",
            err
        );
    }

    #[test]
    fn config_reload_picks_up_new_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("repos.json");

        let repo_a = tmp.path().join("repo-a");
        std::fs::create_dir(&repo_a).unwrap();

        let mut config = ReposConfig::default();
        config
            .register_with_alias(repo_a.clone(), Some("a".to_string()))
            .unwrap();
        config.save_to(&config_file).unwrap();

        let state = ServeState::new(config, Some(config_file.clone()));
        assert_eq!(state.aliases(), vec!["a"]);

        // Add a new alias directly to the file
        let repo_b = tmp.path().join("repo-b");
        std::fs::create_dir(&repo_b).unwrap();
        let mut config2 = ReposConfig::load_from(&config_file).unwrap();
        config2
            .register_with_alias(repo_b, Some("b".to_string()))
            .unwrap();

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
        config
            .register_with_alias(repo_path.clone(), Some("x".to_string()))
            .unwrap();
        config.save_to(&config_file).unwrap();

        let state = ServeState::new(config, Some(config_file.clone()));
        // Open alias x so it lands in DashMap
        let _ = state.get_or_open_stores("x", true).await.unwrap();
        assert!(state.repos.contains_key("x"));

        // Rewrite config without x
        let config2 = ReposConfig::default();

        // Small sleep to ensure mtime changes on Windows
        std::thread::sleep(std::time::Duration::from_millis(150));
        config2.save_to(&config_file).unwrap();

        // Next query for x should fail as unknown
        let err = match state.get_or_open_stores("x", true).await {
            Err(e) => e,
            Ok(_) => panic!("expected unknown alias after removal"),
        };
        assert!(
            err.contains("Unknown alias"),
            "expected unknown alias, got: {}",
            err
        );
        assert!(!state.repos.contains_key("x"));
    }

    #[test]
    fn config_reload_no_spurious_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("repos.json");

        let repo_path = tmp.path().join("myrepo");
        std::fs::create_dir(&repo_path).unwrap();

        let mut config = ReposConfig::default();
        config
            .register_with_alias(repo_path, Some("a".to_string()))
            .unwrap();
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

    /// Verify that the /repos/:alias/reindex route is registered and reachable.
    /// This test starts a real axum server on a random port and sends a POST request.
    #[tokio::test]
    async fn reindex_route_is_registered() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("myrepo");
        std::fs::create_dir(&repo_path).unwrap();

        let mut config = ReposConfig::default();
        config
            .register_with_alias(repo_path.clone(), Some("testalias".to_string()))
            .unwrap();

        let config_file = tmp.path().join("repos.json");
        config.save_to(&config_file).unwrap();

        let state = Arc::new(ServeState::new(config, Some(config_file)));

        let app = axum::Router::new()
            .route(
                crate::constants::HEALTH_PATH,
                axum::routing::get(health_handler),
            )
            .route(
                "/repos/:alias/reindex",
                axum::routing::post(reindex_handler),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // Give the server a moment to start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let client = reqwest::Client::new();

        // POST to unknown alias → 404 from our handler (not axum's built-in 404)
        let resp = client
            .post(format!("http://{}/repos/unknown/reindex", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::NOT_FOUND,
            "expected 404 from our handler"
        );
        let body: serde_json::Value = resp
            .json()
            .await
            .expect("handler should return JSON body for 404");
        assert!(
            body.get("error").is_some(),
            "expected JSON error body, got: {}",
            body
        );

        // POST to known alias → 202 Accepted or 500 (DB missing), but NOT axum's built-in 404
        // The key assertion is that the route IS registered (we get our handler's response, not axum's empty 404)
        let resp = client
            .post(format!("http://{}/repos/testalias/reindex", addr))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.expect("handler should return JSON body");
        assert!(
            status == reqwest::StatusCode::ACCEPTED
                || status == reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "expected 202 or 500 from our handler (not axum's 404), got {}: {}",
            status,
            body
        );
        assert!(
            body.get("status").is_some(),
            "expected JSON with 'status' field, got: {}",
            body
        );
    }

    #[test]
    fn config_reload_tolerates_parse_error() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("repos.json");

        let repo_path = tmp.path().join("myrepo");
        std::fs::create_dir(&repo_path).unwrap();

        let mut config = ReposConfig::default();
        config
            .register_with_alias(repo_path.clone(), Some("a".to_string()))
            .unwrap();
        config.save_to(&config_file).unwrap();

        let state = ServeState::new(config, Some(config_file.clone()));
        assert!(state.aliases().contains(&"a".to_string()));

        // Overwrite with garbage
        std::fs::write(&config_file, "not-json-at-all").unwrap();

        // Should not panic; old config still usable
        let aliases = state.aliases();
        assert!(aliases.contains(&"a".to_string()));
    }

    /// Verify that concurrent reindex requests for the same alias return 409 Conflict.
    #[tokio::test]
    async fn concurrent_reindex_returns_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("myrepo");
        std::fs::create_dir(&repo_path).unwrap();

        let mut config = ReposConfig::default();
        config
            .register_with_alias(repo_path.clone(), Some("testalias".to_string()))
            .unwrap();

        let config_file = tmp.path().join("repos.json");
        config.save_to(&config_file).unwrap();

        let state = Arc::new(ServeState::new(config, Some(config_file)));

        let app = axum::Router::new()
            .route(
                "/repos/:alias/reindex",
                axum::routing::post(reindex_handler),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let client = reqwest::Client::new();

        // First request: 202 Accepted (or 500 if DB missing) — but NOT 409
        let resp1 = client
            .post(format!("http://{}/repos/testalias/reindex", addr))
            .send()
            .await
            .unwrap();
        let status1 = resp1.status();
        assert!(
            status1 == reqwest::StatusCode::ACCEPTED
                || status1 == reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "first request should be 202 or 500, got {}",
            status1
        );

        // If the first request was accepted (202), the reindex is running in background.
        // Send a second request immediately — should get 409 Conflict.
        if status1 == reqwest::StatusCode::ACCEPTED {
            let resp2 = client
                .post(format!("http://{}/repos/testalias/reindex", addr))
                .send()
                .await
                .unwrap();
            assert_eq!(
                resp2.status(),
                reqwest::StatusCode::CONFLICT,
                "second concurrent request should be 409 Conflict"
            );
            let body: serde_json::Value = resp2.json().await.unwrap();
            assert_eq!(body["status"], "conflict");
        }
    }
}