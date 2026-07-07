//! `codesearch serve` — MCP streamable HTTP server mode.
//!
//! Binds on `{host}:{port}` (default `127.0.0.1:39725`) and serves:
//! - `GET /health` → JSON health check
//! - `POST /repos` → register + index + warmup a new repo
//! - `DELETE /repos/:alias` → stop FSW + evict + unregister + delete DB
//! - `POST /repos/:alias/reindex` → trigger incremental or force reindex
//! - MCP streamable HTTP at `/mcp` via rmcp tower service
//!
//! Holds a `DashMap<String, Arc<SharedStores>>` keyed by repo alias.
//! Lazy-opens stores on first query. Conflicted repos are isolated.

mod tui;
mod tui_common;
mod tui_remote;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::response::{IntoResponse, Json as AxumJson};
use colored::Colorize;
use dashmap::DashMap;
use rmcp::transport::{
    streamable_http_server::session::local::LocalSessionManager, StreamableHttpServerConfig,
    StreamableHttpService,
};
use serde_json::json;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::cache::safe_canonicalize;
use crate::constants::{
    ALLOWED_ROOTS_ENV, CHUNK_PATH, CSHARP_PREWARM_ENABLED_ENV, CSHARP_PREWARM_MAX_SYMBOLS,
    CSHARP_SCIP_CONCURRENCY_DEFAULT, CSHARP_SCIP_CONCURRENCY_ENV, DB_DIR_NAME, DEFAULT_SERVE_PORT,
    EXPLORE_PATH, FIND_PATH, HEALTHZ_PATH, HEALTH_PATH, LANG_CSHARP, MAX_INDEXING_SECS,
    MAX_INDEXING_SECS_ENV, MCP_ENDPOINT_PATH, PERSIST_DEBOUNCE_SECS, REAPER_INTERVAL_SECS,
    REMOTES_PATH, REPO_IDLE_TIMEOUT_ENV, REPO_IDLE_TIMEOUT_SECS, SEARCH_PATH, SERVE_API_KEY_ENV,
    SERVE_PORT_ENV, STATUS_PATH,
};
use crate::db_discovery::repos::{config_dir, ReposConfig};
use crate::index::{CSharpRebuildNotifier, IndexManager, IndexingStatusCallback, SharedStores};
use crate::mcp::types::HealthResponse;
use crate::symbols::{csharp, RebuildScope, SymbolIndexerRegistry};

// ---------------------------------------------------------------------------
// Network auth configuration (captured at startup, passed to middleware)
// ---------------------------------------------------------------------------

/// Configuration for the `require_auth_for_network` middleware.
///
/// Captured once at serve startup so the middleware doesn't re-read env vars
/// on every request. Passed via `axum::Extension`.
#[derive(Clone)]
struct NetworkAuthConfig {
    /// Whether the server is bound to a non-localhost address.
    is_network_bind: bool,
    /// The API key to validate (captured from env var at startup).
    /// `None` means no auth (localhost-only mode).
    api_key: Option<String>,
}

/// Check whether a host address is a localhost address.
fn is_localhost_host(host: &str) -> bool {
    host == "127.0.0.1" || host == "::1" || host.eq_ignore_ascii_case("localhost")
}

/// Lightweight repo status label derived from DashMap state only (no DB opens).
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

/// Status of the C# symbol index for a repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CSharpIndexStatus {
    /// No C# helper detected or no index built.
    None,
    /// Helper available and index built successfully.
    Ready,
    /// Index exists but had errors or is stale.
    Error,
    /// Symbol index is currently being built.
    Indexing,
}

/// Lightweight repo status derived from DashMap state only (no DB opens).
pub(crate) struct RepoStatusInfo {
    pub(crate) status: RepoStateLabel,
    pub(crate) changes: u64,
    pub(crate) last_tool_call: Option<String>,
    pub(crate) tool_call_count: u64,
    pub(crate) csharp_index: CSharpIndexStatus,
    pub(crate) csharp_error: Option<String>,
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

/// Result of [`ServeState::try_open_stores`].
///
/// - [`OpenedStores::Write`]: opened in write mode; NOT yet registered in `repos`. Caller decides state.
/// - [`OpenedStores::Readonly`]: opened in readonly mode; ALREADY registered as [`RepoState::Readonly`].
pub(crate) enum OpenedStores {
    Write(Arc<SharedStores>),
    Readonly(Arc<SharedStores>),
}

/// Shared state for the serve mode.
pub(crate) struct ServeState {
    /// Repo alias → opened stores (or conflicted marker).
    repos: DashMap<String, RepoState>,
    /// Repo alias → timestamp of last query that touched this repo.
    /// Used by the idle-reaper to evict repos after `REPO_IDLE_TIMEOUT_SECS`.
    last_access: DashMap<String, std::time::Instant>,
    /// Repo alias → `JoinHandle` of its background file-system-watcher (FSW) task.
    ///
    /// The FSW task holds its own clones of `Arc<SharedStores>` and
    /// `Arc<IndexManager>`. On Windows those Arcs keep the LMDB mmap files
    /// (`data.mdb` / `lock.mdb`) open, which blocks deletion of the DB
    /// directory — the OS refuses to delete a file mmap'd by the very process
    /// asking for the delete. Tracking the handle lets `remove_repo` /
    /// `restart_fsw` await the task's completion (after signalling stop via
    /// `stop_fsw`) so the LMDB `Environment` drops and releases the file
    /// handles BEFORE the DB directory is deleted. See `await_fsw_shutdown`.
    fsw_tasks: DashMap<String, tokio::task::JoinHandle<()>>,
    /// Loaded repos config (alias → path).
    config: std::sync::RwLock<ReposConfig>,
    /// Last observed mtime of the repos config file.
    config_mtime: std::sync::RwLock<Option<std::time::SystemTime>>,
    /// Optional override for the repos config path (used in tests to avoid env vars).
    config_path_override: Option<PathBuf>,
    /// Aliases currently being reindexed — prevents concurrent force reindex
    /// on the same repo. The value is the `Instant` the entry was inserted so
    /// that stale (leaked) entries can be detected and self-healed; see
    /// [`Self::begin_indexing`] / [`Self::is_indexing`] and
    /// [`MAX_INDEXING_SECS`].
    ///
    /// Wrapped in `Arc` so that [`Self::make_indexing_status_callback`] can
    /// capture a cheap clone that **shares** the underlying map (a bare
    /// `DashMap::clone()` is a deep copy and would silently disconnect the
    /// file-watcher callback from this field).
    active_reindexes: Arc<DashMap<String, Instant>>,
    /// Per-repo change count since serve started (incremented by index/reindex operations).
    repo_changes: DashMap<String, AtomicU64>,
    /// Per-repo last tool call: (tool_name, timestamp).
    last_tool_call: DashMap<String, (String, std::time::Instant)>,
    /// Currently active MCP sessions.
    active_sessions: AtomicU64,
    /// Total MCP sessions since serve started.
    total_sessions: AtomicU64,
    /// Shared sysinfo instance for CPU measurement — must persist across calls
    /// so cpu_usage() can compute a delta (first call always returns 0%).
    sysinfo_system: std::sync::Mutex<sysinfo::System>,
    /// Shared symbol indexer registry — used by HTTP reindex handler and MCP
    /// `find_impact` to reuse helper-detection cache instead of creating fresh
    /// instances per request.
    symbol_registry: Arc<SymbolIndexerRegistry>,
    /// Shared embedding service — used by MCP sessions AND the REST handlers so
    /// the ONNX embedding model is loaded ONCE per serve instance (lazily, on
    /// the first semantic query) and reused across all requests. Without this,
    /// per-request `CodesearchService` construction (REST handlers) would reload
    /// the model on every call (~100ms–2s). Mirrors the `symbol_registry` pattern.
    embedding_service: Arc<std::sync::Mutex<Option<crate::embed::EmbeddingService>>>,
    /// Per-repo total tool call count.
    tool_call_counts: DashMap<String, AtomicU64>,
    /// Per-repo C# symbol index status (cached, updated on rebuild/detect).
    /// Wrapped in `Arc` so the watcher-loop notifier closure can capture a cheap clone
    /// without requiring `Arc<ServeState>` in methods that only have `&self`.
    csharp_index_status: Arc<DashMap<String, CSharpIndexStatus>>,
    /// Per-repo C# symbol index last error message (set when status is Error, cleared on success).
    /// Wrapped in `Arc` for the same reason as `csharp_index_status`.
    csharp_index_error: Arc<DashMap<String, String>>,
    /// Debounced deadline for persisting repos config metadata (unix millis).
    persist_deadline_unix_ms: AtomicU64,
    /// Ensures only one debounce worker task runs.
    persist_worker_started: AtomicBool,
    /// Test-only counter for reload invocations that actually swapped config.
    #[cfg(test)]
    reload_count: std::sync::atomic::AtomicUsize,
    /// Instant when ServeState was created — used to compute uptime for TUI header.
    started_at: std::time::Instant,
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

/// Decision returned by [`ServeState::evaluate_csharp_rebuild`].
///
/// Using an enum rather than `&'static str` prevents fragile string
/// comparisons at call sites (previously `reason == "fresh, last_scip>=last_changed"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RebuildDecision {
    /// SCIP index exists and timestamps show no changes since last build.
    Fresh,
    /// No `.sln` file found; C# indexing is not applicable for this repo.
    NoSolutionFile,
    /// The scip-csharp helper binary is not available.
    HelperUnavailable,
    /// An indexing task for this alias is already running.
    AlreadyInFlight,
    /// The config lock was poisoned; retry later.
    ConfigPoisoned,
    /// No SCIP index exists yet; a first build is needed.
    NoIndex,
    /// The repo has changed since the last SCIP build.
    ChangedSinceLastBuild,
}

impl RebuildDecision {
    fn needs_rebuild(self) -> bool {
        matches!(self, Self::NoIndex | Self::ChangedSinceLastBuild)
    }
}

impl ServeState {
    fn new(config: ReposConfig, config_path_override: Option<PathBuf>) -> Self {
        let mut sys = sysinfo::System::new();
        sys.refresh_cpu_list(sysinfo::CpuRefreshKind::nothing());
        Self {
            repos: DashMap::new(),
            last_access: DashMap::new(),
            fsw_tasks: DashMap::new(),
            config: std::sync::RwLock::new(config),
            config_mtime: std::sync::RwLock::new(None),
            config_path_override,
            active_reindexes: Arc::new(DashMap::new()),
            repo_changes: DashMap::new(),
            last_tool_call: DashMap::new(),
            active_sessions: AtomicU64::new(0),
            total_sessions: AtomicU64::new(0),
            sysinfo_system: std::sync::Mutex::new(sys),
            symbol_registry: Arc::new(SymbolIndexerRegistry::new()),
            embedding_service: Arc::new(std::sync::Mutex::new(None)),
            tool_call_counts: DashMap::new(),
            csharp_index_status: Arc::new(DashMap::new()),
            csharp_index_error: Arc::new(DashMap::new()),
            persist_deadline_unix_ms: AtomicU64::new(0),
            persist_worker_started: AtomicBool::new(false),
            #[cfg(test)]
            reload_count: std::sync::atomic::AtomicUsize::new(0),
            started_at: std::time::Instant::now(),
        }
    }

    /// Return a clone of the shared symbol indexer registry Arc.
    /// Used by MCP sessions (CodesearchService::new_for_serve) and
    /// HTTP reindex handler (trigger_symbol_rebuild) to reuse
    /// helper-detection cache instead of creating fresh instances per request.
    pub(crate) fn symbol_registry(&self) -> Arc<SymbolIndexerRegistry> {
        Arc::clone(&self.symbol_registry)
    }

    /// Return a clone of the shared embedding-service Arc.
    /// Shared across MCP sessions AND REST handlers so the ONNX model is loaded
    /// once per serve instance (lazily on first semantic query) instead of being
    /// reloaded per request/session.
    pub(crate) fn embedding_service(
        &self,
    ) -> Arc<std::sync::Mutex<Option<crate::embed::EmbeddingService>>> {
        Arc::clone(&self.embedding_service)
    }

    /// Return the instant when serve started, used to compute uptime.
    pub(crate) fn started_at(&self) -> std::time::Instant {
        self.started_at
    }

    /// Build a `CSharpRebuildNotifier` for the given repo `alias`.
    ///
    /// The notifier captures `Arc` clones of the two status maps so it can be sent
    /// into the file-watcher background task without holding a reference to `&self`.
    /// When the watcher-triggered rebuild completes it calls the closure, which updates
    /// `csharp_index_status` and `csharp_index_error` — making the outcome visible in
    /// the TUI and in `/status` without any extra polling.
    fn make_csharp_notifier(&self, alias: &str) -> CSharpRebuildNotifier {
        let status_map = Arc::clone(&self.csharp_index_status);
        let error_map = Arc::clone(&self.csharp_index_error);
        let alias_key = alias.to_string();
        Arc::new(move |success: bool, error_msg: Option<String>| {
            if success {
                status_map.insert(alias_key.clone(), CSharpIndexStatus::Ready);
                error_map.remove(&alias_key);
            } else {
                if let Some(msg) = error_msg {
                    error_map.insert(alias_key.clone(), msg);
                }
                status_map.insert(alias_key.clone(), CSharpIndexStatus::Error);
            }
        })
    }

    /// Build an `IndexingStatusCallback` for the given repo `alias`.
    ///
    /// The callback captures a clone of `active_reindexes` so it can be sent
    /// into the file-watcher background task. When the watcher triggers a refresh
    /// (branch change, significant batch), it calls this closure to insert/remove
    /// the alias — making "Indexing" visible in the TUI.
    fn make_indexing_status_callback(&self, alias: &str) -> IndexingStatusCallback {
        let reindexes = self.active_reindexes.clone();
        let alias_key = alias.to_string();
        Arc::new(move |active: bool| {
            if active {
                reindexes.insert(alias_key.clone(), Instant::now());
            } else {
                reindexes.remove(&alias_key);
            }
        })
    }

    /// Mark `alias` as actively indexing, returning `true` if the caller may
    /// proceed. Returns `false` only when a **non-stale** entry already exists
    /// (i.e. another reindex is genuinely in progress) — in that case the
    /// caller should return HTTP 409. Stale entries are silently overwritten
    /// with a fresh timestamp.
    ///
    /// This is the guard used by `reindex_handler`, `add_repo_handler`, and
    /// `spawn_force_reindex` to reject concurrent reindexes.
    fn begin_indexing(&self, alias: &str) -> bool {
        let now = Instant::now();
        let max = self.indexing_timeout();
        match self.active_reindexes.entry(alias.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(mut e) => {
                if now.duration_since(*e.get()) < max {
                    // Genuinely in progress — reject.
                    false
                } else {
                    // Stale entry from a leaked/crashed task — overwrite.
                    *e.get_mut() = now;
                    true
                }
            }
            dashmap::mapref::entry::Entry::Vacant(e) => {
                e.insert(now);
                true
            }
        }
    }

    /// Remove the indexing marker for `alias`. Called when a background
    /// indexing task finishes (success, error, or panic).
    fn end_indexing(&self, alias: &str) {
        self.active_reindexes.remove(alias);
    }

    /// Returns `true` if `alias` is currently (non-stale) indexing.
    ///
    /// Stale entries — those older than [`MAX_INDEXING_SECS`] — are lazily
    /// evicted here. This is the self-healing mechanism: even if a
    /// fire-and-forget background task panics or is cancelled between
    /// `begin_indexing` and `end_indexing`, the entry eventually expires and
    /// the TUI returns to the correct state without a server restart.
    ///
    /// The eviction uses the atomic `remove_if` primitive so that a concurrent
    /// `begin_indexing` that inserts a fresh timestamp between the staleness
    /// check and the removal cannot be wrongly evicted.
    fn is_indexing(&self, alias: &str) -> bool {
        let max = self.indexing_timeout();
        // Atomically evict a stale entry. `remove_if` holds the shard's write
        // lock for the predicate check + removal, so a racing `begin_indexing`
        // that refreshed the timestamp in the meantime will cause the predicate
        // to return false and the entry to be kept.
        if self
            .active_reindexes
            .remove_if(alias, |_, ts| ts.elapsed() >= max)
            .is_some()
        {
            tracing::warn!(
                "🧹 Evicted stale indexing marker for '{}' (older than {}s) — \
                 likely a leaked/crashed background task",
                alias,
                max.as_secs()
            );
            return false;
        }
        // Entry is either absent or still within the active window.
        self.active_reindexes.contains_key(alias)
    }

    /// Returns the configured maximum indexing duration, honouring the
    /// `CODESEARCH_MAX_INDEXING_SECS` env override (falls back to
    /// [`MAX_INDEXING_SECS`]).
    fn indexing_timeout(&self) -> Duration {
        std::env::var(MAX_INDEXING_SECS_ENV)
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&secs: &u64| secs > 0)
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(MAX_INDEXING_SECS))
    }

    fn now_unix_secs() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
    }

    fn now_unix_millis() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    /// Read CSHARP_SCIP_CONCURRENCY from env, default
    /// `CSHARP_SCIP_CONCURRENCY_DEFAULT` (currently 2), clamped to [1, 4].
    fn csharp_scip_concurrency() -> usize {
        let raw = std::env::var(CSHARP_SCIP_CONCURRENCY_ENV)
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(CSHARP_SCIP_CONCURRENCY_DEFAULT);
        raw.clamp(1, 4)
    }

    fn has_solution_file(repo_path: &Path) -> bool {
        std::fs::read_dir(repo_path)
            .ok()
            .into_iter()
            .flat_map(|it| it.flatten())
            .any(|entry| entry.path().extension().and_then(|e| e.to_str()) == Some("sln"))
    }

    fn bootstrap_last_changed(repo_path: &Path) -> Option<i64> {
        if repo_path.join(".git").exists() {
            if let Ok(out) = std::process::Command::new("git")
                .arg("-C")
                .arg(repo_path)
                .arg("log")
                .arg("-1")
                .arg("--format=%ct")
                .arg("HEAD")
                .output()
            {
                if out.status.success() {
                    if let Ok(ts) = String::from_utf8_lossy(&out.stdout).trim().parse::<i64>() {
                        return Some(ts);
                    }
                }
            }
        }

        Self::bootstrap_last_changed_via_fs(repo_path)
    }

    fn bootstrap_last_changed_via_fs(repo_path: &Path) -> Option<i64> {
        fn is_ignored_dir(name: &str) -> bool {
            matches!(
                name,
                "bin" | "obj" | "node_modules" | ".git" | ".codesearch.db"
            )
        }
        fn is_candidate(path: &Path) -> bool {
            matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("sln" | "csproj" | "cs")
            )
        }

        let mut stack = vec![repo_path.to_path_buf()];
        let mut scanned = 0usize;
        let mut best: Option<i64> = None;

        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if scanned >= 10_000 {
                    return best;
                }
                scanned += 1;

                let path = entry.path();
                if path.is_dir() {
                    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                        if is_ignored_dir(name) {
                            continue;
                        }
                    }
                    stack.push(path);
                    continue;
                }

                if !is_candidate(&path) {
                    continue;
                }
                let Ok(meta) = std::fs::metadata(&path) else {
                    continue;
                };
                let Ok(modified) = meta.modified() else {
                    continue;
                };
                let secs = modified
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                best = Some(best.map_or(secs, |b| b.max(secs)));
            }
        }

        best
    }

    /// Evaluates whether a C# SCIP rebuild is needed and why.
    ///
    /// Always bootstraps `last_changed_unix` when missing — even for repos
    /// that have no index yet — so phase-2 can sort *all* candidates by
    /// recency. Without this, every first-build candidate has
    /// `last_changed_unix = 0`, making `candidates.sort_by(last_changed)`
    /// effectively a no-op and the actual processing order dictated by
    /// HashMap iteration of `config.repos`. That's why a stale repo could
    /// land first instead of the most-recently-touched one.
    fn evaluate_csharp_rebuild(
        self: &Arc<Self>,
        alias: &str,
        repo_path: &Path,
        db_path: &Path,
    ) -> RebuildDecision {
        if !Self::has_solution_file(repo_path) {
            return RebuildDecision::NoSolutionFile;
        }

        let Some(indexer) = self.symbol_registry.get(LANG_CSHARP) else {
            return RebuildDecision::HelperUnavailable;
        };
        if !indexer.is_available() {
            return RebuildDecision::HelperUnavailable;
        }

        let status = self
            .csharp_index_status
            .get(alias)
            .map(|e| *e.value())
            .unwrap_or(CSharpIndexStatus::None);
        if matches!(status, CSharpIndexStatus::Indexing) {
            return RebuildDecision::AlreadyInFlight;
        }

        // Bootstrap last_changed_unix UP FRONT (before the has_index branch),
        // so the sort key is meaningful for both first-builds and refreshes.
        //
        // IMPORTANT: bootstrap_last_changed spawns a git subprocess and walks
        // the filesystem (≤10,000 entries). Running that work while holding the
        // config write-lock would block every concurrent config.read() call for
        // the duration of the scan. Fix: check whether bootstrapping is needed
        // under a *read* lock, perform the slow I/O outside any lock, then take
        // the write lock only for the brief config update.
        let needs_bootstrap = {
            let cfg = match self.config.read() {
                Ok(c) => c,
                Err(_) => return RebuildDecision::ConfigPoisoned,
            };
            cfg.meta(alias).last_changed_unix.is_none()
        };
        // Slow git/fs work runs here — no lock held.
        let bootstrapped_ts = if needs_bootstrap {
            Self::bootstrap_last_changed(repo_path).or_else(|| Some(Self::now_unix_secs()))
        } else {
            None
        };

        let (last_changed, last_scip, touched_bootstrap) = {
            let mut cfg = match self.config.write() {
                Ok(c) => c,
                Err(_) => return RebuildDecision::ConfigPoisoned,
            };
            let mut meta = cfg.meta(alias);
            let mut touched = false;
            if meta.last_changed_unix.is_none() {
                // Another task may have bootstrapped between our read and write;
                // the double-check here is intentional (TOCTOU-safe via the write lock).
                meta.last_changed_unix = bootstrapped_ts;
                if let Some(ts) = meta.last_changed_unix {
                    touched = cfg.touch_last_changed(alias, ts);
                }
            }
            (
                meta.last_changed_unix.unwrap_or(0),
                meta.last_scip_indexed_unix.unwrap_or(0),
                touched,
            )
        };

        if touched_bootstrap {
            self.schedule_persist_repos_config();
        }

        if !indexer.has_index(db_path) {
            return RebuildDecision::NoIndex;
        }

        if last_changed > last_scip {
            RebuildDecision::ChangedSinceLastBuild
        } else {
            RebuildDecision::Fresh
        }
    }

    pub(crate) fn schedule_persist_repos_config(self: &Arc<Self>) {
        let deadline = Self::now_unix_millis() + (PERSIST_DEBOUNCE_SECS * 1000);
        self.persist_deadline_unix_ms
            .store(deadline, Ordering::Relaxed);

        if self
            .persist_worker_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        let state = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let deadline = state.persist_deadline_unix_ms.load(Ordering::Relaxed);
                if deadline == 0 {
                    continue;
                }
                let now = Self::now_unix_millis();
                if now < deadline {
                    continue;
                }

                let cfg = match state.config.read() {
                    Ok(c) => c.clone(),
                    Err(e) => {
                        tracing::warn!("repos persist skipped: config lock poisoned: {}", e);
                        state.persist_deadline_unix_ms.store(0, Ordering::Relaxed);
                        continue;
                    }
                };
                // Route through persist_config so the override path is honored
                // (keeps the metadata-persist worker hermetic in tests; identical
                // to cfg.save() in production where the override is None).
                let state_persist = state.clone();
                let save_res =
                    tokio::task::spawn_blocking(move || state_persist.persist_config(&cfg)).await;
                match save_res {
                    Ok(Ok(())) => tracing::debug!("repos.json metadata persisted"),
                    Ok(Err(e)) => tracing::warn!("repos persist failed: {}", e),
                    Err(e) => tracing::warn!("repos persist task join failed: {}", e),
                }

                state.persist_deadline_unix_ms.store(0, Ordering::Relaxed);
                state.persist_worker_started.store(false, Ordering::Release);

                // Avoid race where a schedule landed between save() and worker stop.
                let pending = state.persist_deadline_unix_ms.load(Ordering::Acquire);
                if pending > Self::now_unix_millis()
                    && state
                        .persist_worker_started
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    continue;
                }
                break;
            }
        });
    }

    /// Reconcile registered repo paths against the filesystem before warmup.
    ///
    /// For each alias whose stored path no longer exists (folder renamed/moved),
    /// attempt a best-effort git-identity relocation and rewrite `repos.json`.
    /// When relocation fails the entry is left in place and merely logged — it is
    /// skipped safely at warmup and never crashes serve. Explicit cleanup of
    /// unrecoverable entries is available via `codesearch index prune`.
    pub(crate) fn reconcile_all_paths(self: &Arc<Self>) {
        let aliases = self.aliases();
        if aliases.is_empty() {
            return;
        }

        let mut config = match self.config.write() {
            Ok(c) => c,
            Err(e) => {
                warn!("reconcile: config lock poisoned: {}", e);
                return;
            }
        };

        let (relocated, unresolved) = config.relocate_missing();

        for (alias, new_path) in &relocated {
            info!("reconcile: relocated '{}' → {}", alias, new_path.display());
        }
        for alias in &unresolved {
            let missing = config
                .repos
                .get(alias)
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            warn!(
                "reconcile: '{}' path missing ({}); skipping — \
                 run `codesearch index prune` to remove it",
                alias, missing
            );
        }

        if !relocated.is_empty() {
            if let Err(e) = self.persist_config(&config) {
                warn!("reconcile: failed to persist relocated paths: {}", e);
            }
        }
    }

    /// Phase 1: warm all repos sequentially, awaiting incremental refresh per repo.
    /// Stale entries (database missing or repo path gone) are auto-pruned from repos.json.
    pub(crate) async fn run_phase_1_warmup_all(self: &Arc<Self>) {
        let aliases = self.aliases();
        if aliases.is_empty() {
            return;
        }
        info!("🔥 Phase 1 warmup: {} repos (no FSW)", aliases.len());

        let mut pruned: Vec<String> = Vec::new();

        for alias in &aliases {
            match self.warmup_repo(alias).await {
                Ok(()) => info!("phase-1: warmed '{}'", alias),
                Err(e) => {
                    // Auto-prune stale entries where the database or repo path no longer exists.
                    let should_prune = {
                        let config = self.config.read().ok();
                        let path = config.as_ref().and_then(|c| c.resolve(alias));
                        match path {
                            Some(p) => {
                                let db_missing = !p.join(DB_DIR_NAME).exists();
                                let path_gone = !p.exists();
                                db_missing || path_gone
                            }
                            None => {
                                // Alias resolves to nothing — definitely stale
                                true
                            }
                        }
                    };

                    if should_prune {
                        warn!("phase-1: pruning stale alias '{}' — {}", alias, e);
                        // Clean up any residual in-memory state
                        let _ = self.stop_fsw(alias);
                        self.repos.remove(alias);
                        self.last_access.remove(alias);
                        // Detach the FSW handle (stop_fsw already cancelled
                        // the task). The DB dir is already gone (the prune
                        // condition was db_missing || path_gone), so there is
                        // no delete to race with — the task just drains.
                        self.fsw_tasks.remove(alias);

                        // Unregister from repos.json — route through persist_config
                        // so the config_path_override is honoured (same as all
                        // other save sites in ServeState).
                        if let Ok(mut config) = self.config.write() {
                            if config.unregister_alias(alias) {
                                if let Err(save_err) = self.persist_config(&config) {
                                    warn!(
                                        "phase-1: failed to save repos.json after pruning '{}': {}",
                                        alias, save_err
                                    );
                                } else {
                                    pruned.push(alias.clone());
                                }
                            }
                        }
                    } else {
                        warn!("phase-1: warmup '{}' failed: {}", alias, e);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        if !pruned.is_empty() {
            info!(
                "🔥 Phase 1 warmup complete (pruned {} stale: {})",
                pruned.len(),
                pruned.join(", ")
            );
        } else {
            info!("🔥 Phase 1 warmup complete");
        }
    }

    /// Phase 2: semaphore-bounded concurrent C# SCIP rebuilds, sorted by recency.
    pub(crate) async fn run_phase_2_csharp_scip(self: &Arc<Self>) {
        let aliases = self.aliases();
        let mut candidates: Vec<(String, i64)> = Vec::new();

        for alias in &aliases {
            let path = match self.config.read().ok().and_then(|c| c.resolve(alias)) {
                Some(p) => p,
                None => continue,
            };
            let db_path = path.join(DB_DIR_NAME);
            // evaluate_csharp_rebuild may spawn a git subprocess and walk the
            // filesystem — offload to the blocking pool so the async runtime
            // stays responsive while processing all candidates.
            let state2 = self.clone();
            let alias2 = alias.clone();
            let path2 = path.clone();
            let db_path2 = db_path.clone();
            let decision = match tokio::task::spawn_blocking(move || {
                state2.evaluate_csharp_rebuild(&alias2, &path2, &db_path2)
            })
            .await
            {
                Ok(d) => d,
                Err(e) => {
                    warn!(
                        "phase-2: evaluate_csharp_rebuild panicked for '{}': {:?}",
                        alias, e
                    );
                    continue;
                }
            };
            if !decision.needs_rebuild() {
                // If the SCIP index exists and is fresh, mark C# status as Ready
                // so the TUI shows the C# indicator (e.g. "C#·") instead of None.
                if decision == RebuildDecision::Fresh {
                    let mut status = self
                        .csharp_index_status
                        .get(alias)
                        .map(|e| *e.value())
                        .unwrap_or(CSharpIndexStatus::None);
                    if matches!(status, CSharpIndexStatus::None) {
                        status = CSharpIndexStatus::Ready;
                    }
                    self.csharp_index_status.insert(alias.to_string(), status);
                }
                info!("phase-2: skip '{}' — {:?}", alias, decision);
                continue;
            }
            let last_changed = self
                .config
                .read()
                .ok()
                .and_then(|c| c.meta(alias).last_changed_unix)
                .unwrap_or(0);
            info!(
                "phase-2: queued '{}' — {:?} (last_changed={})",
                alias, decision, last_changed
            );
            candidates.push((alias.clone(), last_changed));
        }

        candidates.sort_by_key(|b| std::cmp::Reverse(b.1));
        if candidates.is_empty() {
            info!("phase-2 complete: 0 candidates");
            return;
        }

        // Pre-mark all queued candidates as C# Indexing so the TUI C# indicator
        // reflects pending rebuilds immediately, even before each repo acquires
        // its semaphore slot. trigger_symbol_rebuild will overwrite this with the
        // same value (no-op) and eventually with Ready or Error on completion.
        for (alias, _) in &candidates {
            self.csharp_index_status
                .insert(alias.clone(), CSharpIndexStatus::Indexing);
        }

        let concurrency = Self::csharp_scip_concurrency();
        info!(
            "phase-2: {} candidates, concurrency={}",
            candidates.len(),
            concurrency
        );
        let sem = Arc::new(Semaphore::new(concurrency));
        let mut handles = Vec::with_capacity(candidates.len());

        for (alias, _) in candidates {
            let sem = sem.clone();
            let state = self.clone();
            handles.push(tokio::spawn(async move {
                let permit = match sem.acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => return,
                };
                info!("phase-2: starting '{}'", alias);
                let path = match state.config.read().ok().and_then(|c| c.resolve(&alias)) {
                    Some(p) => p,
                    None => {
                        drop(permit);
                        return;
                    }
                };
                // Guard against stale entries whose folder was removed/renamed
                // and could not be relocated: skip rather than run SCIP on a
                // non-existent path.
                if !path.exists() {
                    warn!(
                        "phase-2: skip '{}' — path missing ({})",
                        alias,
                        path.display()
                    );
                    drop(permit);
                    return;
                }
                let db_path = path.join(DB_DIR_NAME);
                trigger_symbol_rebuild(&alias, &path, &db_path, &state).await;
                drop(permit);
            }));
        }

        for handle in handles {
            let _ = handle.await;
        }
        info!("phase-2 complete");
    }

    /// Phase 3: background pre-warm of reference cache for C# repos.
    ///
    /// After Phase 2 finishes indexing definitions, Phase 3 runs
    /// `scip-csharp batch-find-refs` for each repo to resolve references
    /// for all uncached symbols in a single workspace session.
    /// This amortizes the 30-60s workspace open cost across thousands of
    /// symbols, making subsequent `find_impact` calls instant (LMDB cache hit).
    ///
    /// Controlled by `CSHARP_PREWARM_ENABLED` env (default: "true").
    /// Set to "false" to skip Phase 3 entirely (useful on memory-constrained machines).
    pub(crate) async fn run_phase_3_prewarm(self: &Arc<Self>) {
        let enabled = std::env::var(CSHARP_PREWARM_ENABLED_ENV)
            .unwrap_or_else(|_| "true".to_string())
            .parse::<bool>()
            .unwrap_or(true);

        if !enabled {
            info!(
                "phase-3: pre-warm disabled by {}=false",
                CSHARP_PREWARM_ENABLED_ENV
            );
            return;
        }

        let aliases = self.aliases();
        let mut candidates: Vec<String> = Vec::new();

        for alias in &aliases {
            let path = match self.config.read().ok().and_then(|c| c.resolve(alias)) {
                Some(p) => p,
                None => continue,
            };

            // Skip stale entries whose folder no longer exists.
            if !path.exists() {
                continue;
            }

            // Only pre-warm repos that have a ready C# index
            let status = self
                .csharp_index_status
                .get(alias)
                .map(|g| *g.value())
                .unwrap_or(CSharpIndexStatus::None);

            if !matches!(status, CSharpIndexStatus::Ready) {
                info!("phase-3: skip '{}' — C# status is {:?}", alias, status);
                continue;
            }

            // Check that the repo is applicable
            let applies = self
                .symbol_registry
                .get(LANG_CSHARP)
                .map(|i| i.applies_to(&path))
                .unwrap_or(false);

            if !applies {
                continue;
            }

            candidates.push(alias.clone());
        }

        if candidates.is_empty() {
            info!("phase-3: 0 candidates for pre-warm");
            return;
        }

        info!("phase-3: pre-warming {} repo(s)", candidates.len());

        let concurrency = Self::csharp_scip_concurrency();
        let sem = Arc::new(Semaphore::new(concurrency));
        let mut handles = Vec::with_capacity(candidates.len());

        for alias in candidates {
            let sem = sem.clone();
            let state = self.clone();
            handles.push(tokio::spawn(async move {
                let _permit = match sem.acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => return,
                };

                info!("phase-3: pre-warming '{}'", alias);

                let path = match state.config.read().ok().and_then(|c| c.resolve(&alias)) {
                    Some(p) => p,
                    None => return,
                };
                let db_path = path.join(DB_DIR_NAME);

                let registry = state.symbol_registry.clone();
                let alias_owned = alias.clone();

                // Signal TUI: C# ref-cache pre-warm is in progress.
                // We set csharp_index_status → Indexing so the C# column shows "C#…"
                // without touching active_reindexes — pre-warm does not block HTTP
                // /reindex requests and should not override the repo label (Warm/Open).
                state
                    .csharp_index_status
                    .insert(alias_owned.clone(), CSharpIndexStatus::Indexing);

                match tokio::task::spawn_blocking(move || {
                    let Some(indexer) = registry.get(LANG_CSHARP) else {
                        return Err(anyhow::anyhow!("No C# indexer"));
                    };
                    // Downcast to CSharpSymbolIndexer to access prewarm_ref_cache
                    let csharp_indexer = indexer
                        .as_any()
                        .downcast_ref::<csharp::CSharpSymbolIndexer>()
                        .ok_or_else(|| {
                            anyhow::anyhow!("Failed to downcast to CSharpSymbolIndexer")
                        })?;
                    csharp_indexer.prewarm_ref_cache(&path, &db_path, CSHARP_PREWARM_MAX_SYMBOLS)
                })
                .await
                {
                    Ok(Ok(summary)) => {
                        info!(
                            "phase-3: pre-warm complete for '{}': {} resolved, {} cached in {}ms",
                            alias_owned, summary.resolved, summary.cached, summary.duration_ms
                        );
                    }
                    Ok(Err(e)) => {
                        tracing::warn!("phase-3: pre-warm failed for '{}': {}", alias_owned, e);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "phase-3: pre-warm task panicked for '{}': {}",
                            alias_owned,
                            e
                        );
                    }
                }

                // Restore Ready status regardless of pre-warm outcome: the SCIP
                // definitions index (built in Phase 2) remains valid even if
                // ref-cache pre-warm fails. TUI returns to "C#·" (ready).
                state
                    .csharp_index_status
                    .insert(alias_owned, CSharpIndexStatus::Ready);
            }));
        }

        for handle in handles {
            let _ = handle.await;
        }
        info!("phase-3 complete");
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

        // Canonicalize to resolve symlinks, prevent path traversal, and strip
        // Windows UNC prefix (\\?\) so paths compare correctly against stored values.
        // CodeQL: path derives from env var (CODESEARCH_REPOS_CONFIG) — validate before use.
        let config_path = match safe_canonicalize(&config_path) {
            Ok(p) => p,
            Err(_) => return Ok(()), // file doesn't exist yet — nothing to reload
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
            // Warm, Readonly, Conflicted just drop.
            // Also detach the FSW task handle so it doesn't leak across a config
            // shrink. Reload never deletes DB dirs, so a still-draining task
            // holding LMDB open briefly is harmless (no delete to race with).
            self.fsw_tasks.remove(alias);
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

    /// Remove a repo: stop FSW, evict from memory, unregister from config, delete DB.
    ///
    /// This is the shared logic used by both the HTTP `DELETE /repos/:alias` handler
    /// and the TUI confirmation flow.
    pub(crate) async fn remove_repo(&self, alias: &str) -> Result<()> {
        // 1. Resolve project path from config
        let project_path = {
            let config = self
                .config
                .read()
                .map_err(|e| anyhow::anyhow!("Config lock poisoned: {}", e))?;
            config
                .resolve(alias)
                .ok_or_else(|| anyhow::anyhow!("Unknown alias '{}'", alias))?
        };

        let db_path = project_path.join(DB_DIR_NAME);

        // 2. Stop FSW and evict from memory.
        {
            let stores = self.stop_fsw(alias);
            drop(stores);
        }
        self.repos.remove(alias);
        self.last_access.remove(alias);
        // Await the FSW task's exit so its Arc<SharedStores>/Arc<IndexManager>
        // clones drop → the LMDB Environment closes → Windows releases the mmap
        // file handles BEFORE we delete the DB directory below. stop_fsw above
        // already cancelled the task; this waits for it to actually finish.
        self.await_fsw_shutdown(alias).await;
        tracing::info!("Evicted repo '{}' from memory", alias);

        // 3. Unregister from repos.json
        {
            let mut config = self
                .config
                .write()
                .map_err(|e| anyhow::anyhow!("Config lock poisoned: {}", e))?;
            config.unregister_alias(alias);
            if let Err(e) = self.persist_config(&config) {
                tracing::warn!(
                    "Failed to save repos config after removing '{}': {}",
                    alias,
                    e
                );
            }
        }

        // 4. Delete the database directory with retries.
        //
        // `await_fsw_shutdown` above dropped the *persistent* holders (the FSW
        // task + this RepoState), which is the fix for the Windows
        // sharing-violation. A *transient* holder can still defeat a single
        // delete attempt: a search / warmup in flight at this instant may hold
        // its own clone of the Arc<SharedStores> (or an inner
        // Arc<RwLock<VectorStore>> captured in a spawn_blocking), keeping the
        // LMDB env open past the await. The retry-loop below is the fallback
        // for that race; if it still fails (warned, non-fatal) the DB dir
        // stays on disk and is cleaned up on the next serve restart. The repo
        // is already unregistered from config, so this is cosmetic.
        if db_path.exists() {
            for attempt in 0..5 {
                if attempt > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                }
                match std::fs::remove_dir_all(&db_path) {
                    Ok(()) => {
                        tracing::info!("Deleted database for '{}': {}", alias, db_path.display());
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
                RepoState::Readonly { .. } | RepoState::Conflicted => {
                    // Cannot force-reindex a readonly or conflicted store.
                    // Drop the entry from the DashMap so the LMDB TrackedEnv
                    // is released — the caller will reopen in write mode via
                    // try_open_stores(allow_create=true).
                    drop(entry);
                    self.close_repo(alias);
                    return None;
                }
            }
        }
        None
    }

    /// Remove a repo from the DashMap, dropping its stores and releasing
    /// LMDB file handles. Used before force-reindex reopen.
    fn close_repo(&self, alias: &str) {
        if self.repos.remove(alias).is_some() {
            tracing::info!(
                "Closed repo '{}' (dropped stores, released LMDB handles)",
                alias
            );
        }
    }

    /// Await the completion of a repo's background FSW task (if any) and drop
    /// its handle.
    ///
    /// MUST be called AFTER the task has been signalled to stop — i.e. the
    /// caller has already invoked [`Self::stop_fsw`] (which cancels the
    /// `CancellationToken`) or otherwise cancelled the token. Once the task
    /// observes the cancellation and returns, the `Arc<SharedStores>` /
    /// `Arc<IndexManager>` clones it holds are dropped → the LMDB
    /// `Environment` drops synchronously (`mdb_env_close`) → Windows releases
    /// the mmap file handles → the DB directory can be deleted.
    ///
    /// Bounded to 5 s so a stuck task can never wedge `remove_repo`; on
    /// timeout the handle is dropped (detaching the task) and a warning is
    /// logged. The DB delete retry-loop in `remove_repo` remains as a
    /// fallback for that edge case.
    async fn await_fsw_shutdown(&self, alias: &str) {
        if let Some((_, handle)) = self.fsw_tasks.remove(alias) {
            match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
                Ok(Ok(())) => {
                    tracing::debug!("FSW task for '{}' exited cleanly", alias);
                }
                Ok(Err(join_err)) => {
                    tracing::warn!(
                        "FSW task for '{}' panicked during shutdown: {}",
                        alias,
                        join_err
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        "FSW task for '{}' did not exit within 5s; LMDB handles may stay locked",
                        alias
                    );
                }
            }
        }
    }

    /// Spawn the FSW background task for a repo after it has been stopped.
    ///
    /// Creates a fresh IndexManager, performs an initial incremental refresh,
    /// then starts the continuous file watcher loop. Updates the RepoState with
    /// the new cancel token and IndexManager.
    async fn restart_fsw(&self, alias: &str, stores: Arc<SharedStores>) {
        // The caller already cancelled the previous FSW task via stop_fsw.
        // Await its exit so its Arc<SharedStores>/Arc<IndexManager> clones drop
        // before we spawn a new task against the same stores (and so the old
        // handle is removed from fsw_tasks before we insert a fresh one below).
        self.await_fsw_shutdown(alias).await;

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
                let notifier = self.make_csharp_notifier(alias);
                let indexing_cb = self.make_indexing_status_callback(alias);

                let fsw_handle = tokio::spawn(async move {
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

                    if let Err(e) = im_for_task
                        .start_file_watcher(token_for_task, Some(notifier), Some(indexing_cb))
                        .await
                    {
                        tracing::error!("File watcher for '{}' stopped: {}", alias_bg, e);
                    }
                });

                self.fsw_tasks.insert(alias.to_string(), fsw_handle);

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
    pub(crate) async fn warmup_repo(
        self: &Arc<Self>,
        alias: &str,
    ) -> std::result::Result<(), String> {
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

        // Open stores: existence check + write/readonly/conflicted logic.
        let stores = match self.try_open_stores(alias, &db_path, false)? {
            OpenedStores::Readonly(_) => {
                // Already registered as Readonly by try_open_stores.
                // Touch so the idle reaper can evict this handle.
                self.touch_access(alias);
                return Ok(());
            }
            OpenedStores::Write(s) => s,
        };

        // Build vector index from existing data.
        //
        // `build_index()` is a synchronous, CPU-heavy operation (HNSW graph
        // construction). Running it directly on a tokio worker thread starves
        // the async executor and makes `/health` time out during warmup, so it
        // is offloaded to `spawn_blocking`. Stats are read first under a short
        // `.read()` lock to decide whether a build is even needed.
        let needs_build = {
            let vstore = stores.vector_store.read().await;
            match vstore.stats() {
                Ok(s) if s.total_chunks > 0 && !s.indexed => Some(s.total_chunks),
                Ok(_) => None,
                Err(e) => {
                    warn!("Warmup '{}': could not read stats: {}", alias, e);
                    None
                }
            }
        };
        if let Some(total_chunks) = needs_build {
            info!(
                "Warmup '{}': building vector index ({} existing chunks)",
                alias, total_chunks
            );
            let vector_store = Arc::clone(&stores.vector_store);
            match tokio::task::spawn_blocking(move || {
                let mut vstore = vector_store.blocking_write();
                vstore.build_index()
            })
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    warn!("Warmup '{}': failed to build vector index: {}", alias, e)
                }
                Err(e) => warn!(
                    "Warmup '{}': build vector index task panicked: {}",
                    alias, e
                ),
            }
        }

        let stores_arc = stores;

        if let Err(e) =
            IndexManager::perform_incremental_refresh_with_stores(&path, &db_path, &stores_arc)
                .await
        {
            tracing::warn!("Warmup '{}': incremental refresh failed: {}", alias, e);
        }

        // Store as Warm — FSW will be started lazily on first query.
        self.repos
            .insert(alias.to_string(), RepoState::Warm { stores: stores_arc });

        // Start the idle timer at warmup. A real query will reset it via
        // touch_access; without this, repos that are warmed but never queried
        // would never appear in `last_access` and therefore never be evicted
        // by `evict_idle_repos`, holding LMDB envs and embedder state forever.
        self.touch_access(alias);
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

        // Open stores: existence check + write/readonly/conflicted logic.
        let stores = match self.try_open_stores(alias, &db_path, false)? {
            OpenedStores::Readonly(s) => {
                // Already registered as Readonly; touch and return.
                self.touch_access(alias);
                return Ok(s);
            }
            OpenedStores::Write(s) => s,
        };

        // Ensure the HNSW vector index is built from existing data.
        // When opening an existing DB, VectorStore starts with indexed=false.
        // Without this, search fails with "Index not built" until the background
        // refresh completes (which may take minutes for large repos).
        // build_index() is CPU-heavy — offload to the blocking pool so the async
        // runtime is not stalled while building the HNSW index for large repos.
        {
            let vector_store = Arc::clone(&stores.vector_store);
            let alias_owned = alias.to_string();
            match tokio::task::spawn_blocking(move || {
                let mut vstore = vector_store.blocking_write();
                match vstore.stats() {
                    Ok(s) if s.total_chunks > 0 && !s.indexed => {
                        info!(
                            "Building vector index for '{}' ({} existing chunks)",
                            alias_owned, s.total_chunks
                        );
                        if let Err(e) = vstore.build_index() {
                            warn!("Failed to build vector index for '{}': {}", alias_owned, e);
                        }
                    }
                    Ok(_) => {} // already indexed or no chunks
                    Err(e) => warn!("Could not read stats for '{}': {}", alias_owned, e),
                }
            })
            .await
            {
                Ok(()) => {}
                Err(e) => warn!("warmup: build_index task panicked for '{}': {:?}", alias, e),
            }
        }

        let stores_arc = stores;

        // Fan-out / candidate-detection callers pass touch=false.
        // Open as Warm only — no FSW, no IndexManager overhead.
        // Always update last_access so the reaper can evict this repo after idle timeout.
        if !touch {
            self.repos.insert(
                alias.to_string(),
                RepoState::Warm {
                    stores: stores_arc.clone(),
                },
            );
            self.touch_access(alias);
            return Ok(stores_arc);
        }

        // Explicit project query (touch=true) — start FSW, full Write mode.
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
                    let notifier = self.make_csharp_notifier(alias);
                    let indexing_cb = self.make_indexing_status_callback(alias);

                    let fsw_handle = tokio::spawn(async move {
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
                        if let Err(e) = im_for_task
                            .start_file_watcher(token_for_task, Some(notifier), Some(indexing_cb))
                            .await
                        {
                            tracing::error!("File watcher for '{}' stopped: {}", alias_clone, e);
                        }
                    });
                    self.fsw_tasks.insert(alias.to_string(), fsw_handle);

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
        // touch=true is guaranteed here (fan-out returns early above as Warm).
        self.touch_access(alias);
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
        let notifier = self.make_csharp_notifier(alias);
        let indexing_cb = self.make_indexing_status_callback(alias);

        let cancel_token = CancellationToken::new();
        let token_for_task = cancel_token.clone();

        // Fire-and-forget: create IndexManager + start FSW in background.
        // We don't block the first query — the repo is already searchable from the Warm state.
        let fsw_handle = tokio::spawn(async move {
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

                    if let Err(e) = im_for_task
                        .start_file_watcher(token_for_task, Some(notifier), Some(indexing_cb))
                        .await
                    {
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
        self.fsw_tasks.insert(alias.to_string(), fsw_handle);

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

    /// Opens (or creates) LMDB/Tantivy stores for the repo at db_path.
    ///
    /// `allow_create=false`: warmup / incremental reindex path — fails if DB is missing.
    /// `allow_create=true`:  force-reindex / add-repo path — creates fresh DB if missing.
    fn try_open_stores(
        &self,
        alias: &str,
        db_path: &Path,
        allow_create: bool,
    ) -> std::result::Result<OpenedStores, String> {
        if !db_path.exists() && !allow_create {
            let parent = db_path
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            return Err(format!(
                "Database not found at {}. This usually means the repo was removed externally. \
                 Run `codesearch index add {}` to recreate, or `codesearch index rm {}` to clean up the config entry.",
                db_path.display(),
                parent,
                parent
            ));
        }

        let dims = self.get_dimensions_for_path(db_path);

        match SharedStores::new(db_path, dims) {
            Ok(s) => {
                info!("Opened repo in write mode: {}", alias);
                Ok(OpenedStores::Write(Arc::new(s)))
            }
            Err(write_err) => {
                if allow_create {
                    return Err(format!(
                        "Failed to open/create database for {}: {}",
                        alias, write_err
                    ));
                }
                match SharedStores::new_readonly(db_path, dims) {
                    Ok(s) => {
                        info!("Opened repo in readonly mode: {}", alias);
                        let stores_arc = Arc::new(s);
                        self.repos.insert(
                            alias.to_string(),
                            RepoState::Readonly {
                                stores: stores_arc.clone(),
                            },
                        );
                        Ok(OpenedStores::Readonly(stores_arc))
                    }
                    Err(e) => {
                        warn!("Failed to open repo {}: {}", alias, e);
                        self.repos.insert(alias.to_string(), RepoState::Conflicted);
                        Err(Self::conflicted_msg(alias))
                    }
                }
            }
        }
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

    /// Persist the repos config to disk, honoring `config_path_override`.
    ///
    /// Handlers MUST call this instead of `ReposConfig::save()` directly, so that
    /// writes land in the same file `reload_if_changed` reads from. In production
    /// `config_path_override` is `None`, making this identical to `config.save()`
    /// (the real `~/.codesearch/repos.json`). In tests the override points at a
    /// temp file, which keeps the register/remove paths hermetic and lets us
    /// assert on persistence without touching the user's real config.
    pub(crate) fn persist_config(&self, config: &ReposConfig) -> anyhow::Result<()> {
        match self.config_path_override.as_ref() {
            Some(path) => config.save_to(path),
            None => config.save(),
        }
    }

    /// Resolve a group name to its constituent aliases.
    /// Returns an error if the group doesn't exist.
    ///
    /// The reserved name `ALL_GROUP_NAME` ("all") is a virtual group: it is never
    /// stored in `repos.json` but resolves dynamically to every registered alias.
    pub(crate) fn resolve_group_aliases(
        &self,
        group: &str,
    ) -> std::result::Result<Vec<String>, String> {
        let _ = self.reload_if_changed();
        let config = match self.config.read() {
            Ok(c) => c,
            Err(e) => return Err(format!("Config lock poisoned: {}", e)),
        };
        // Virtual "all" group: resolve to every registered alias (sorted for
        // deterministic ordering). Not stored in repos.json.
        if group == crate::constants::ALL_GROUP_NAME {
            let mut all: Vec<String> = config.repos.keys().cloned().collect();
            all.sort();
            return Ok(all);
        }
        config
            .groups
            .get(group)
            .cloned()
            .ok_or_else(|| format!("Unknown group '{}'", group))
    }

    /// Record that a repo was just accessed (query or reindex).
    /// Called from `get_or_open_stores(touch=true)`, `reindex_handler`,
    /// `add_repo_handler`, and `warmup_repo` (after successful warmup).
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
        // Increment total call count
        self.tool_call_counts
            .entry(alias.to_string())
            .and_modify(|c| {
                c.fetch_add(1, Ordering::Relaxed);
            })
            .or_insert_with(|| AtomicU64::new(1));
    }

    /// Most recent real tool-call time across all repos, if any.
    ///
    /// Used by the cloud keep-warm task to decide whether the server is still
    /// "active". Only genuine tool calls update `last_tool_call`; health/status
    /// probes and the keep-warm self-ping do not, so this reflects real query
    /// activity — not the keep-warm traffic that keeps the replica alive.
    pub(crate) fn most_recent_tool_call(&self) -> Option<Instant> {
        self.last_tool_call
            .iter()
            .map(|entry| entry.value().1)
            .max()
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

            let label = if self.is_indexing(alias) {
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

            let tool_call_count = self
                .tool_call_counts
                .get(alias)
                .map(|c| c.load(Ordering::Relaxed))
                .unwrap_or(0);

            // C# index status: check cached value first, then probe
            let csharp_index = self
                .csharp_index_status
                .get(alias)
                .map(|e| *e.value())
                .unwrap_or_else(|| {
                    // Probe: helper available + index exists → Ready
                    let registry = &self.symbol_registry;
                    let has_helper = registry
                        .get(LANG_CSHARP)
                        .map(|i| i.is_available())
                        .unwrap_or(false);
                    if has_helper && registry.has_index_for(LANG_CSHARP, &db_path) {
                        CSharpIndexStatus::Ready
                    } else {
                        CSharpIndexStatus::None
                    }
                });

            let csharp_error = if matches!(csharp_index, CSharpIndexStatus::Error) {
                self.csharp_index_error
                    .get(alias)
                    .map(|e: dashmap::mapref::one::Ref<String, String>| e.value().clone())
            } else {
                None
            };

            result.push((
                alias.clone(),
                RepoStatusInfo {
                    status: label,
                    changes,
                    last_tool_call: last_tool,
                    tool_call_count,
                    csharp_index,
                    csharp_error,
                },
            ));
        }
        // Sort alphabetically by alias for consistent display in TUI
        result.sort_by_key(|a| a.0.to_ascii_lowercase());
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
                if self.is_indexing(alias) {
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
            // Detach the FSW handle (no-op for Warm/Readonly/Conflicted — they
            // have no FSW task). Eviction frees memory; the DB dir is NOT
            // deleted (the repo can be re-opened on the next query), so we
            // don't need to await — the cancelled task drains on its own.
            self.fsw_tasks.remove(alias);
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

/// Unauthenticated liveness-probe handler: GET /healthz
///
/// Always returns `200 {"status":"ok"}` with no version or repo information.
/// Exempted from `require_auth_for_network`, so container-orchestrator probes
/// (e.g. Azure Container Apps) can reach it on a network bind without the
/// Bearer key. Keep this body free of any sensitive/identifying data.
async fn healthz_handler() -> AxumJson<serde_json::Value> {
    AxumJson(json!({ "status": "ok" }))
}

/// Status handler: GET /status
///
/// Returns a JSON snapshot of all repo states, active sessions, and CPU usage.
/// Used by the standalone TUI (`codesearch serve tui`) to poll server state.
async fn status_handler(
    axum::extract::State(state): axum::extract::State<Arc<ServeState>>,
) -> AxumJson<serde_json::Value> {
    let repos = state.repo_statuses_lightweight();
    let active_sessions = state.active_session_count();

    let repo_json: Vec<serde_json::Value> = repos
        .iter()
        .map(|(alias, info)| {
            let status_str = match info.status {
                RepoStateLabel::Open => "open",
                RepoStateLabel::Warm => "warm",
                RepoStateLabel::Readonly => "readonly",
                RepoStateLabel::Closed => "closed",
                RepoStateLabel::Indexing => "indexing",
                RepoStateLabel::Error => "error",
                RepoStateLabel::NoIndex => "no_index",
            };
            let lock_mode = match info.status {
                RepoStateLabel::Open | RepoStateLabel::Indexing => "write",
                RepoStateLabel::Warm | RepoStateLabel::Readonly => "read",
                _ => "—",
            };
            let csharp_str = match info.csharp_index {
                CSharpIndexStatus::None => "none",
                CSharpIndexStatus::Ready => "ready",
                CSharpIndexStatus::Error => "error",
                CSharpIndexStatus::Indexing => "indexing",
            };
            json!({
                "alias": alias,
                "status": status_str,
                "lock_mode": lock_mode,
                "changes": info.changes,
                "last_tool_call": info.last_tool_call,
                "tool_call_count": info.tool_call_count,
                "csharp_index": csharp_str,
                "csharp_error": info.csharp_error,
            })
        })
        .collect();

    let uptime_secs = state.started_at().elapsed().as_secs();

    // CPU usage — reuse shared System instance so cpu_usage() can compute delta
    let cpu = {
        use sysinfo::ProcessesToUpdate;
        let pid = match sysinfo::get_current_pid() {
            Ok(p) => p,
            Err(_) => {
                return AxumJson(json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "repos": repo_json,
                    "active_sessions": active_sessions,
                    "cpu_percent": "—",
                    "uptime_secs": uptime_secs,
                }));
            }
        };
        let mut sys = match state.sysinfo_system.lock() {
            Ok(s) => s,
            Err(_) => {
                return AxumJson(json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "repos": repo_json,
                    "active_sessions": active_sessions,
                    "cpu_percent": "—",
                    "uptime_secs": uptime_secs,
                }));
            }
        };
        sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        match sys.process(pid) {
            Some(proc) => {
                let num_cpus = sys.cpus().len().max(1) as f32;
                let pct = proc.cpu_usage() / num_cpus;
                format!("{:.0}%", pct)
            }
            None => "—".to_string(),
        }
    };

    let csharp_helper = state
        .symbol_registry
        .get(LANG_CSHARP)
        .map(|i| i.is_available())
        .unwrap_or(false);

    AxumJson(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "repos": repo_json,
        "active_sessions": active_sessions,
        "cpu_percent": cpu,
        "csharp_helper": csharp_helper,
        "uptime_secs": uptime_secs,
    }))
}

/// Projection of a federation peer that is safe to expose over `GET /remotes`.
///
/// This is a **dedicated, deliberately narrow type** rather than a reuse of
/// [`crate::db_discovery::repos::RemotePeer`]: `RemotePeer` carries the
/// `api_key` shared secret, which must NEVER leave the process via this
/// observability endpoint. By construction this struct has no `api_key` field,
/// so the secret cannot be serialized even by accident. Only the four
/// operator-relevant fields are projected here.
#[derive(serde::Serialize)]
struct RemotePeerInfo {
    alias: String,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_secs: Option<u64>,
}

/// Remotes handler: GET /remotes
///
/// Observability companion to [`status_handler`]: lists the federation peers
/// this serve fans out to (the `remotes` map from `repos.json`), sorted by
/// alias for stable output. On a config read error the endpoint degrades
/// gracefully to `{"remotes": []}` rather than returning a 500, since the peer
/// list is purely informational.
///
/// Read-only and status-like: same auth policy as `/status` (no admin key on
/// localhost, protected by `require_auth_for_network` on network binds). Does
/// not touch in-memory [`ServeState`], so it takes no state extractor.
async fn remotes_handler() -> AxumJson<serde_json::Value> {
    // Load the on-disk peer config; a read failure is non-fatal for an
    // observability endpoint — report an empty peer list instead of a 500.
    let cfg = crate::db_discovery::load_repos_config().unwrap_or_default();

    // Project each peer into the api_key-less `RemotePeerInfo` view, then sort
    // by alias for deterministic output.
    let mut remotes: Vec<RemotePeerInfo> = cfg
        .remotes
        .iter()
        .map(|(alias, p)| RemotePeerInfo {
            alias: alias.clone(),
            url: p.url.clone(),
            group: p.group.clone(),
            timeout_secs: p.timeout_secs,
        })
        .collect();
    remotes.sort_by(|a, b| a.alias.cmp(&b.alias));

    AxumJson(json!({ "remotes": remotes }))
}

/// Info handler: GET /repos/{alias}/info
///
/// Returns live index stats for a single repo, mirroring the TUI info overlay
/// (`tui::build_info_overlay`). Used by the remote TUI's `i` key.
async fn info_handler(
    axum::extract::Path(alias): axum::extract::Path<String>,
    axum::extract::State(state): axum::extract::State<Arc<ServeState>>,
) -> axum::response::Response {
    use axum::http::StatusCode;

    let config = state.config_snapshot();
    let project_path = match config.resolve(&alias) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                AxumJson(json!({
                    "error": format!("unknown repo alias: {}", alias),
                    "status": "error",
                })),
            )
                .into_response();
        }
    };
    let db_path = project_path.join(DB_DIR_NAME);

    // Defaults (overridden by metadata.json then live store stats).
    let mut chunks = 0usize;
    let mut files = 0usize;
    let mut max_chunk_id = 0u32;
    let mut dims = 0usize;
    let mut model = String::from("unknown");
    let mut lock = String::from("—");
    let mut index_age = String::from("—");

    // Read model + dims + counts from metadata.json.
    if let Ok(content) = std::fs::read_to_string(db_path.join("metadata.json")) {
        if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&content) {
            model = meta
                .get("model_short_name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            dims = meta.get("dimensions").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            chunks = meta
                .get("total_chunks")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            files = meta
                .get("total_files")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            if let Some(indexed_at) = meta.get("indexed_at").and_then(|v| v.as_str()) {
                index_age = tui::format_age(indexed_at);
            }
        }
    }

    // If stores are open, live stats override metadata.
    if let Some(stores) = state.get_opened_stores(&alias) {
        if let Ok(vs) = stores.vector_store.try_read() {
            if let Ok(live_stats) = vs.stats() {
                chunks = live_stats.total_chunks;
                files = live_stats.total_files;
                max_chunk_id = live_stats.max_chunk_id;
                if dims == 0 {
                    dims = live_stats.dimensions;
                }
            }
        }
        lock = if stores.readonly {
            "read".to_string()
        } else {
            "write".to_string()
        };
    }

    let db_size_human = tui::dir_size_human(&db_path);

    AxumJson(json!({
        "chunks": chunks,
        "files": files,
        "max_chunk_id": max_chunk_id,
        "db_size_human": db_size_human,
        "model": model,
        "dims": dims,
        "lock": lock,
        "index_age": index_age,
    }))
    .into_response()
}

/// Doctor handler: POST /repos/{alias}/doctor
///
/// Runs doctor diagnostics for a single repo and returns the rendered TUI lines.
/// Reuses the open LMDB handle via `diagnose_with_store` when stores are open,
/// to avoid double-opening the environment. Used by the remote TUI's `d` key.
async fn doctor_handler(
    axum::extract::Path(alias): axum::extract::Path<String>,
    axum::extract::State(state): axum::extract::State<Arc<ServeState>>,
) -> axum::response::Response {
    use axum::http::StatusCode;

    let config = state.config_snapshot();
    let project_path = match config.resolve(&alias) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                AxumJson(json!({
                    "error": format!("unknown repo alias: {}", alias),
                    "status": "error",
                })),
            )
                .into_response();
        }
    };

    // diagnose() walks the repo tree and scans LMDB — synchronous, potentially
    // slow work. Run it on a blocking thread so it never occupies a tokio worker
    // (mirrors the reindex path). Reuse the open LMDB handle when available
    // (via blocking_read inside the blocking task) to avoid double-opening the
    // env; otherwise diagnose() opens its own read-only handle.
    let opened = state.get_opened_stores(&alias);
    let pp = project_path.clone();
    let report = tokio::task::spawn_blocking(move || match opened {
        Some(stores) => {
            let vs = stores.vector_store.blocking_read();
            crate::cli::doctor::diagnose_with_store(&pp, &vs)
        }
        None => crate::cli::doctor::diagnose(&pp),
    })
    .await
    .unwrap_or_else(|e| Err(anyhow::anyhow!("doctor task panicked: {}", e)));

    let results = match report {
        Ok(report) => report.render_tui(),
        Err(e) => vec![
            format!("✗ Doctor failed: {}", e),
            String::new(),
            "  [Esc] close".to_string(),
        ],
    };

    AxumJson(json!({ "results": results })).into_response()
}

/// Trigger a symbol index rebuild for a repo (C# etc.).
///
/// Reuses the shared `SymbolIndexerRegistry` from `ServeState`, looks up the C# indexer,
/// and runs `rebuild()` in a blocking task. Updates the C# index status on success/failure.
async fn trigger_symbol_rebuild(
    alias: &str,
    project_path: &Path,
    db_path: &Path,
    state: &Arc<ServeState>,
) {
    // Skip non-applicable repos BEFORE touching status — otherwise the TUI
    // would flip C#-indicator red on Rust/Python repos that simply have no
    // .sln. The phase-2 gate (`evaluate_csharp_rebuild`) already filters
    // these out, but other callers (POST /reindex?symbols=true,
    // .cs watcher debounce, future paths) bypass that gate.
    let applies = state
        .symbol_registry
        .get(LANG_CSHARP)
        .map(|i| i.applies_to(project_path))
        .unwrap_or(false);
    if !applies {
        tracing::info!(
            "🔬 symbol reindex skipped for '{}': not applicable (no .sln)",
            alias
        );
        return;
    }

    tracing::info!("🔬 symbol reindex triggered for '{}'", alias);
    state
        .csharp_index_status
        .insert(alias.to_string(), CSharpIndexStatus::Indexing);
    // Mark as actively indexing so the TUI status column shows "Indexing"
    // (not just the C# indicator). This mirrors what reindex_handler does.
    //
    // Known benign race: if the FSW-SCIP rebuild path (indexing_cb) fires for
    // the same alias simultaneously, both paths insert into active_reindexes.
    // Because the map key is the alias, there is no data corruption.
    // However, whichever path finishes first will call remove(), which may
    // briefly flip the TUI back to Warm/Open while the other path is still
    // running. This is a cosmetic flash only — no state is corrupted.
    // (Stale entries from a crashed task self-heal via `is_indexing`.)
    state.begin_indexing(alias);
    let rp = project_path.to_path_buf();
    let dp = db_path.to_path_buf();
    let alias_owned = alias.to_string();
    let registry = state.symbol_registry.clone();
    match tokio::task::spawn_blocking(move || {
        let Some(indexer) = registry.get(LANG_CSHARP) else {
            return Err(anyhow::anyhow!("No C# symbol indexer registered"));
        };
        if !indexer.is_available() {
            return Err(anyhow::anyhow!("scip-csharp helper not available"));
        }
        indexer.rebuild(&rp, &dp, RebuildScope::Full)
    })
    .await
    {
        Ok(Ok(summary)) => {
            tracing::info!(
                "✅ Symbol rebuild complete for '{}': {} symbols, {} refs in {}ms",
                alias_owned,
                summary.symbols_indexed,
                summary.references_stored,
                summary.duration_ms
            );
            state.end_indexing(&alias_owned);
            state
                .csharp_index_status
                .insert(alias_owned.clone(), CSharpIndexStatus::Ready);
            state.csharp_index_error.remove(&alias_owned);

            if let Ok(mut cfg) = state.config.write() {
                cfg.touch_last_scip(&alias_owned, ServeState::now_unix_secs());
            }
            state.schedule_persist_repos_config();
        }
        Ok(Err(e)) => {
            tracing::error!("❌ Symbol rebuild failed for '{}': {}", alias_owned, e);
            state.end_indexing(&alias_owned);
            state
                .csharp_index_error
                .insert(alias_owned.clone(), e.to_string());
            state
                .csharp_index_status
                .insert(alias_owned, CSharpIndexStatus::Error);
        }
        Err(e) => {
            tracing::error!(
                "❌ Symbol rebuild task panicked for '{}': {}",
                alias_owned,
                e
            );
            state.end_indexing(&alias_owned);
            state
                .csharp_index_error
                .insert(alias_owned.clone(), format!("Task panicked: {}", e));
            state
                .csharp_index_status
                .insert(alias_owned, CSharpIndexStatus::Error);
        }
    }
}

/// Reload repos config handler: POST /reload
///
/// Forces a reload of repos.json from disk, even if the mtime hasn't changed.
/// Used by the TUI [s] key to pick up external changes (e.g. `codesearch index add`).
async fn reload_handler(
    axum::extract::State(state): axum::extract::State<Arc<ServeState>>,
) -> (
    axum::http::StatusCode,
    axum::response::Json<serde_json::Value>,
) {
    use axum::http::StatusCode;

    // Clear the stored mtime so reload_if_changed will actually reload.
    if let Ok(mut mtime_guard) = state.config_mtime.write() {
        *mtime_guard = None;
    }

    match state.reload_if_changed() {
        Ok(()) => (
            StatusCode::OK,
            axum::response::Json(json!({"status": "reloaded"})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::response::Json(json!({"error": format!("reload failed: {}", e)})),
        ),
    }
}

/// Reindex handler: POST /repos/{alias}/reindex
///
/// Query params:
/// - `force=true` — close the repo, delete the DB, full reindex, reopen.
///   Required when the caller wants a clean rebuild (e.g. `codesearch index -f`).
///   Without force, performs an incremental refresh only.
/// - `symbols=true` — also rebuild the symbol index (C# SCIP) after text reindex.
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

    let symbols = params
        .get("symbols")
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
    if !state.begin_indexing(&alias_bg) {
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

    let do_symbols = symbols;

    if force {
        // Force rebuild: stop FSW -> clear data in-place -> full reindex -> restart FSW.
        // The FSW must be stopped before clearing the FileMetaStore, otherwise it
        // sees all the file writes during reindex as "new changes" and triggers
        // endless incremental refresh cycles.

        // 1. Stop the FSW (cancel its token)
        let stores = match state.stop_fsw(&alias) {
            Some(s) => s,
            None => {
                // FSW not running -- open existing or create fresh DB.
                // allow_create=true so a force-reindex can recover a deleted DB.
                let cancel = CancellationToken::new();
                match state.try_open_stores(&alias, &db_path, true) {
                    Ok(OpenedStores::Write(s)) => {
                        // Register as Write to block double-open races while we reindex.
                        state.repos.insert(
                            alias.clone(),
                            RepoState::Write {
                                stores: s.clone(),
                                index_manager: None,
                                cancel_token: cancel,
                            },
                        );
                        state.touch_access(&alias);
                        s
                    }
                    Ok(OpenedStores::Readonly(_)) => {
                        // Cannot force-reindex against a readonly store.
                        state.end_indexing(&guard_alias);
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            axum::response::Json(json!({
                                "error": format!(
                                    "Repo {} could only be opened read-only; cannot force-reindex",
                                    alias
                                ),
                                "status": "error"
                            })),
                        );
                    }
                    Err(e) => {
                        state.end_indexing(&guard_alias);
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
            match IndexManager::force_reindex_with_stores(&project_path, &db_path, &stores, None)
                .await
            {
                Ok(()) => {
                    tracing::info!("Force reindex complete for '{}'", alias_bg);
                }
                Err(e) => {
                    tracing::error!("Force reindex failed for '{}': {}", alias_bg, e);
                }
            }

            // 3. Restart FSW with fresh IndexManager
            g_state.restart_fsw(&g_alias, stores).await;

            // 4. Optional symbol index rebuild
            if do_symbols {
                trigger_symbol_rebuild(&alias_bg, &project_path, &db_path, &g_state).await;
            }

            g_state.end_indexing(&g_alias);
        });
    } else {
        // Incremental refresh: ensure the repo is opened, then refresh
        let stores = match state.get_or_open_stores(&alias, true).await {
            Ok(s) => s,
            Err(e) => {
                state.end_indexing(&guard_alias);
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

            // Optional symbol index rebuild
            if do_symbols {
                trigger_symbol_rebuild(&alias_bg, &project_path, &db_path, &g_state).await;
            }

            g_state.end_indexing(&g_alias);
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
    /// Optional embedding model override (e.g., "bge-small", "nomic-v1.5").
    model: Option<String>,
}

/// Add-repo handler: POST /repos
///
/// Registers a new repo in repos.json, opens the LMDB/Tantivy stores inline
/// (fast — prevents the double-open race from the old `index_quiet` path),
/// then spawns a full reindex + vector index build + FSW start in the
/// background. Returns 202 Accepted immediately.
async fn add_repo_handler(
    axum::extract::State(state): axum::extract::State<Arc<ServeState>>,
    axum::extract::Json(body): axum::extract::Json<AddRepoRequest>,
) -> (
    axum::http::StatusCode,
    axum::response::Json<serde_json::Value>,
) {
    use axum::http::StatusCode;

    // Canonicalize the path
    let canonical_path = match safe_canonicalize(&body.path) {
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

    // Validate the path is within allowed roots (if configured)
    if let Err(e) = validate_path_within_allowed_roots(&canonical_path) {
        warn!("Rejected repo registration: {}", e);
        return (
            StatusCode::FORBIDDEN,
            axum::response::Json(json!({
                "error": e,
                "status": "forbidden"
            })),
        );
    }

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

        if let Err(e) = state.persist_config(&config) {
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

    // Open stores INLINE (fast -- just creates dirs + opens LMDB/Tantivy handles).
    // This eliminates the LMDB double-open race that occurred when the old
    //  path opened its own LMDB handle, conflicting with
    //  calls from the serve's request handlers.
    let db_path = canonical_path.join(DB_DIR_NAME);
    let stores = match state.try_open_stores(&alias, &db_path, true) {
        Ok(OpenedStores::Write(s)) => s,
        Ok(OpenedStores::Readonly(_)) => {
            unreachable!("try_open_stores(allow_create=true) never returns Readonly")
        }
        Err(e) => {
            // Clean up the config entry we just added
            if let Ok(mut config) = state.config.write() {
                config.unregister_alias(&alias);
                if let Err(e) = state.persist_config(&config) {
                    tracing::warn!(
                        "Failed to persist config after add-repo DB open failure for '{}': {}",
                        alias,
                        e
                    );
                }
            }
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::response::Json(json!({
                    "error": format!("Failed to open database for {}: {}", alias, e),
                    "status": "error"
                })),
            );
        }
    };

    // Store as Write immediately so get_or_open_stores() finds the repo in the
    // fast-path and does NOT try to open a second LMDB handle on the same path.
    let cancel_token = CancellationToken::new();
    state.repos.insert(
        alias.clone(),
        RepoState::Write {
            stores: stores.clone(),
            index_manager: None,
            cancel_token: cancel_token.clone(),
        },
    );
    state.touch_access(&alias);

    // Guard against concurrent reindex for the same alias.
    if !state.begin_indexing(&alias) {
        // Another reindex for this alias is already in progress.
        // We must undo *all* side-effects created so far:
        //   1. Cancel the token and remove from repos (releases the LMDB handle).
        //   2. Unregister from config — the alias was persisted to repos.json a
        //      few lines above. Without this cleanup the alias would remain in
        //      repos.json with no open stores until the server is restarted.
        cancel_token.cancel();
        state.repos.remove(&alias);
        if let Ok(mut config) = state.config.write() {
            config.unregister_alias(&alias);
            if let Err(e) = state.persist_config(&config) {
                tracing::warn!(
                    "Failed to persist config after add-repo conflict for '{}': {}",
                    alias,
                    e
                );
            }
        }
        return (
            StatusCode::CONFLICT,
            axum::response::Json(json!({
                "error": format!("Reindex already in progress for '{}'", alias),
                "status": "conflict"
            })),
        );
    }

    // Parse optional model override from request body.
    let model_override: Option<crate::embed::ModelType> = match body.model.as_deref() {
        Some(model_str) => match crate::embed::ModelType::parse(model_str) {
            Some(mt) => Some(mt),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::response::Json(json!({
                        "error": format!("Unknown model: '{}'. Use one of: {}", model_str, crate::embed::ModelType::valid_short_names()),
                        "status": "error"
                    })),
                );
            }
        },
        None => None,
    };

    // Spawn the heavy indexing work in the background.  Returns 202 immediately.
    let alias_bg = alias.clone();
    let state_bg = state.clone();
    let project_path = canonical_path.clone();

    tokio::spawn(async move {
        tracing::info!(
            "Indexing newly added repo '{}' ({}) in background",
            alias_bg,
            project_path.display()
        );

        match IndexManager::force_reindex_with_stores(
            &project_path,
            &db_path,
            &stores,
            model_override,
        )
        .await
        {
            Ok(()) => {
                tracing::info!(
                    "Index created for '{}' ({})",
                    alias_bg,
                    project_path.display()
                );
            }
            Err(e) => {
                tracing::error!("Index creation failed for '{}': {}", alias_bg, e);
                // Clean up: remove from repos and config
                state_bg.repos.remove(&alias_bg);
                state_bg.end_indexing(&alias_bg);
                if let Ok(mut config) = state_bg.config.write() {
                    config.unregister_alias(&alias_bg);
                    if let Err(e) = state_bg.persist_config(&config) {
                        tracing::warn!(
                            "Failed to persist config after add-repo index failure for '{}': {}",
                            alias_bg,
                            e
                        );
                    }
                }
                return;
            }
        }

        // Build vector index from freshly indexed data.
        // build_index() is CPU-heavy — offload to the blocking pool.
        {
            let vector_store = Arc::clone(&stores.vector_store);
            let alias_bi = alias_bg.clone();
            match tokio::task::spawn_blocking(move || {
                let mut vstore = vector_store.blocking_write();
                vstore.build_index()
            })
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!("Failed to build vector index for '{}': {}", alias_bi, e)
                }
                Err(e) => tracing::warn!("build_index task panicked for '{}': {:?}", alias_bi, e),
            }
        }

        // Start FSW and transition to proper Write state with IndexManager
        state_bg.restart_fsw(&alias_bg, stores).await;

        state_bg.end_indexing(&alias_bg);
        tracing::info!("Repo '{}' fully indexed and ready", alias_bg);
    });

    (
        StatusCode::ACCEPTED,
        axum::response::Json(json!({
            "status": "accepted",
            "alias": alias,
            "path": canonical_path,
            "message": "Repo registered, indexing in background"
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

    match state.remove_repo(&alias).await {
        Ok(()) => {
            let project_path = state.config.read().ok().and_then(|c| c.resolve(&alias));
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
        Err(e) => {
            let msg = e.to_string();
            let status = if msg.contains("Unknown alias") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (
                status,
                axum::response::Json(json!({
                    "error": msg,
                    "status": if status == StatusCode::NOT_FOUND { "not_found" } else { "error" }
                })),
            )
        }
    }
}

/// Validate that a canonical path falls under one of the allowed root directories.
///
/// When `CODESEARCH_ALLOWED_ROOTS` is unset or empty, all paths are allowed (backward compatible).
/// When set, the path must start with at least one of the semicolon-separated roots.
/// All comparisons use canonicalized paths with consistent separators.
///
/// Returns `Ok(())` if the path is allowed, or an error message describing the rejection.
fn validate_path_within_allowed_roots(canonical_path: &Path) -> std::result::Result<(), String> {
    let allowed_roots = match std::env::var(ALLOWED_ROOTS_ENV) {
        Ok(v) if !v.is_empty() => v,
        _ => return Ok(()), // No restriction configured
    };

    let roots: Vec<PathBuf> = allowed_roots
        .split(';')
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();

    if roots.is_empty() {
        return Ok(());
    }

    // Canonicalize each root for reliable comparison (ignores roots that don't exist)
    let canonical_roots: Vec<PathBuf> = roots
        .into_iter()
        .filter_map(|r| safe_canonicalize(&r).ok())
        .collect();

    if canonical_roots.is_empty() {
        // All configured roots failed to canonicalize — reject for safety
        return Err(format!(
            "No valid roots found in {} — all configured paths failed to canonicalize",
            ALLOWED_ROOTS_ENV
        ));
    }

    // Path::starts_with returns true for exact match too (a path starts with itself),
    // so no separate equality check is needed.
    let allowed = canonical_roots
        .iter()
        .any(|root| canonical_path.starts_with(root));

    if allowed {
        Ok(())
    } else {
        let roots_display: Vec<String> = canonical_roots
            .iter()
            .map(|r| r.display().to_string())
            .collect();
        Err(format!(
            "Path '{}' is outside allowed roots: [{}]. Set {} to include this path or leave it empty to allow all paths.",
            canonical_path.display(),
            roots_display.join(", "),
            ALLOWED_ROOTS_ENV
        ))
    }
}

/// Constant-time API key comparison.
///
/// Hashes both the supplied candidate and the configured key with SHA-256 and
/// compares the fixed-length 32-byte digests byte-for-byte without early exit.
/// Because the digests are always the same length and SHA-256 is
/// preimage-resistant, the comparison time does not depend on how many leading
/// bytes of the candidate match the secret — closing the timing side-channel
/// that a raw `==` on the key strings would open on the network-exposed path.
fn api_key_matches(candidate: &str, configured: &str) -> bool {
    use sha2::{Digest, Sha256};
    let candidate_digest = Sha256::digest(candidate.as_bytes());
    let configured_digest = Sha256::digest(configured.as_bytes());
    let mut diff: u8 = 0;
    for (a, b) in candidate_digest.iter().zip(configured_digest.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Returns true if the request carries a valid API key in either the
/// `Authorization: Bearer <key>` or `X-API-Key: <key>` header, compared in
/// constant time against `configured`. Shared by both auth middlewares so the
/// constant-time guarantee lives in exactly one place.
fn request_has_valid_api_key(headers: &axum::http::HeaderMap, configured: &str) -> bool {
    if let Some(auth_val) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(bearer_key) = auth_val.strip_prefix("Bearer ") {
            if api_key_matches(bearer_key, configured) {
                return true;
            }
        }
    }
    if let Some(key_val) = headers.get("X-API-Key").and_then(|v| v.to_str().ok()) {
        if api_key_matches(key_val, configured) {
            return true;
        }
    }
    false
}

/// Axum middleware that requires API key authentication for management endpoints.
///
/// When `CODESEARCH_SERVE_API_KEY` is set, requests must include the key in either:
/// - `Authorization: Bearer <key>` header
/// - `X-API-Key: <key>` header
///
/// When the env var is unset or empty, all requests pass through (backward compatible).
///
/// Management endpoints are: `POST /repos`, `DELETE /repos/:alias`,
/// `POST /repos/:alias/reindex`, `POST /reload`.
/// All other routes (health, status, MCP) are always unauthenticated.
///
/// Key comparison is constant-time (see `api_key_matches`).
async fn require_admin_auth(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = req.uri().path();
    let method = req.method().clone();

    // Only management endpoints require auth.
    let is_management = matches!(path, "/repos" | "/reload")
        || path.ends_with("/reindex")
        || (path.starts_with("/repos/") && method == axum::http::Method::DELETE);

    if !is_management {
        return next.run(req).await;
    }

    let configured_key = match std::env::var(SERVE_API_KEY_ENV) {
        Ok(k) if !k.is_empty() => k,
        _ => return next.run(req).await,
    };

    if request_has_valid_api_key(req.headers(), &configured_key) {
        return next.run(req).await;
    }

    warn!(
        "Rejected unauthenticated management request: {} {}",
        method, path
    );

    (
        axum::http::StatusCode::UNAUTHORIZED,
        axum::response::Json(json!({
            "error": "Unauthorized: valid API key required",
            "status": "unauthorized"
        })),
    )
        .into_response()
}

/// Axum middleware that requires API key authentication for ALL endpoints
/// when the server is bound to a non-localhost address.
///
/// This is the network-level auth layer — it runs before `require_admin_auth`
/// and protects MCP, health, status, and every other route.
/// Uses the same `CODESEARCH_SERVE_API_KEY` env var and the same
/// `Authorization: Bearer <key>` / `X-API-Key: <key>` header pattern.
///
/// Configuration is captured at startup via `NetworkAuthConfig` extension,
/// so env-var changes after startup have no effect (defense in depth).
///
/// When the server is bound to localhost (default), this middleware passes
/// all requests through (backward compatible).
async fn require_auth_for_network(
    axum::Extension(auth_config): axum::Extension<NetworkAuthConfig>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // Public liveness probe: always unauthenticated, even on a network bind.
    // Container orchestrators (e.g. Azure Container Apps) hit this without the
    // Bearer key. The handler returns no sensitive info.
    if req.uri().path() == HEALTHZ_PATH {
        return next.run(req).await;
    }

    // Localhost binding: no auth required.
    if !auth_config.is_network_bind {
        return next.run(req).await;
    }

    let configured_key = match &auth_config.api_key {
        Some(k) => k,
        None => {
            // Should never happen — startup check refuses non-localhost without key.
            // But defense in depth: reject if key somehow missing.
            warn!("Network bind without API key — rejecting request");
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::response::Json(json!({
                    "error": "Server misconfiguration: API key required but not configured",
                    "status": "error"
                })),
            )
                .into_response();
        }
    };

    if request_has_valid_api_key(req.headers(), configured_key) {
        return next.run(req).await;
    }

    let path = req.uri().path();
    let method = req.method().clone();
    warn!(
        "Rejected unauthenticated request from network: {} {}",
        method, path
    );

    (
        axum::http::StatusCode::UNAUTHORIZED,
        axum::response::Json(json!({
            "error": "Unauthorized: API key required for network access",
            "status": "unauthorized"
        })),
    )
        .into_response()
}

/// Axum middleware: log MCP requests (method + path, skips /health spam).
async fn log_mcp_requests(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    let response = next.run(req).await;

    if path != crate::constants::HEALTH_PATH && path != crate::constants::HEALTHZ_PATH {
        let status = response.status().as_u16();
        tracing::info!("{} {} → {}", method, path, status);
    }

    response
}

/// Run the standalone TUI that connects to a running serve instance via HTTP.
///
/// This is the entry point for `codesearch serve tui`.
pub async fn run_tui_standalone(serve_url: String) -> Result<()> {
    if !tui::is_tty() {
        eprintln!("Error: No TTY detected. The standalone TUI requires an interactive terminal.");
        std::process::exit(1);
    }

    // Check if serve is reachable
    let health_url = format!("{}{}", serve_url, HEALTH_PATH);
    match reqwest::get(&health_url).await {
        Ok(resp) if resp.status().is_success() => {}
        Ok(_) => {
            eprintln!(
                "Error: Serve at {} returned an error. Is it running?",
                serve_url
            );
            std::process::exit(1);
        }
        Err(_) => {
            eprintln!(
                "Error: No serve running at {}. Start with: codesearch serve",
                serve_url
            );
            std::process::exit(1);
        }
    }

    tui_remote::run_remote_tui(serve_url).await
}

/// Run the MCP serve mode.
///
/// This is the entry point called from CLI when `codesearch serve` is invoked.
pub async fn run_serve(
    host: Option<String>,
    port: Option<u16>,
    register_paths: Vec<PathBuf>,
    no_tui: bool,
    keep_warm_url: Option<String>,
    idle_suspend_secs: Option<u64>,
    cancel_token: CancellationToken,
) -> Result<()> {
    use crate::constants::{resolve_serve_host, SERVE_HOST_ENV};

    let effective_host = host.unwrap_or_else(resolve_serve_host);

    let effective_port = port.unwrap_or_else(|| {
        std::env::var(SERVE_PORT_ENV)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_SERVE_PORT)
    });

    // ── Security check: non-localhost binding requires API key ──
    let is_localhost = is_localhost_host(&effective_host);

    if !is_localhost {
        let api_key = std::env::var(SERVE_API_KEY_ENV)
            .ok()
            .filter(|k| !k.is_empty());
        if api_key.is_none() {
            anyhow::bail!(
                "Refusing to bind to '{}' without authentication. \
                 Set the {} environment variable to an API key before binding to a non-localhost address. \
                 This protects MCP and all other endpoints from unauthorized network access.",
                effective_host,
                SERVE_API_KEY_ENV
            );
        }
    }

    // Capture auth config at startup for the middleware.
    let network_auth = NetworkAuthConfig {
        is_network_bind: !is_localhost,
        api_key: std::env::var(SERVE_API_KEY_ENV)
            .ok()
            .filter(|k| !k.is_empty()),
    };

    // Load repos config (register any --register paths first)
    let mut config = ReposConfig::load().unwrap_or_default();
    for path in &register_paths {
        let canonical = safe_canonicalize(path).unwrap_or_else(|_| path.clone());

        // Validate path against allowed roots (if configured)
        if let Err(e) = validate_path_within_allowed_roots(&canonical) {
            anyhow::bail!("Rejected --register path '{}': {}", path.display(), e);
        }

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

    // Construct the bind address from resolved host + port.
    // Using `format!` with `parse::<SocketAddr>()` handles both IPv4 and IPv6.
    // For IPv6 literals, users should pass e.g. `[::1]` (with brackets).
    let addr: SocketAddr = format!("{}:{}", effective_host, effective_port)
        .parse()
        .map_err(|e| {
            anyhow::anyhow!(
                "Invalid bind address '{}:{}': {}. \
                 For IPv6, use brackets: `[::1]:port`. \
                 Override with --host or {}.",
                effective_host,
                effective_port,
                e,
                SERVE_HOST_ENV
            )
        })?;

    // Log startup
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
            // the tool handlers via serve_state. Marking it session-tracked pairs
            // the session_connected() above with session_disconnected() in Drop —
            // per-request REST services (make_service) are NOT marked, so they
            // never decrement active_sessions (which would underflow it to MAX).
            let mut svc = crate::mcp::CodesearchService::new_for_serve(state_for_factory.clone())
                .map_err(std::io::Error::other)?;
            svc.mark_session_tracked();
            Ok(svc)
        };

    // Build session manager without keep_alive timeout. The default rmcp timeout
    // (5 min) kills idle sessions too aggressively for a local long-running serve.
    // We run single-user local, so abandoned sessions cost nothing — let TCP
    // liveness determine when a session is truly dead.
    let mut session_manager = LocalSessionManager::default();
    session_manager.session_config.keep_alive = None;
    let session_manager = Arc::new(session_manager);
    let config = StreamableHttpServerConfig::default();

    let mcp_service = StreamableHttpService::new(service_factory, session_manager, config);

    // Build axum router with request logging and optional admin auth.
    // Stale-session recovery is handled client-side by the stdio proxy's retry
    // loop in `McpProxyService` (see src/mcp/mod.rs). Remote MCP clients that
    // are not spec-compliant must reconnect themselves — we do not attempt a
    // server-side transparent reconnect because that path opened a session leak
    // and could not actually reach OpenCode (TCP keep-alive failure happens
    // before the request hits this middleware).
    //
    // Layer order (outermost → innermost):
    //   Extension(inject NetworkAuthConfig) → require_auth_for_network → log_mcp_requests → require_admin_auth → handler.
    // - `Extension`: injects NetworkAuthConfig captured at startup into every request.
    // - `require_auth_for_network`: protects ALL routes when non-localhost (network mode).
    // - `log_mcp_requests`: logs method + path for every request.
    // - `require_admin_auth`: protects management endpoints only (when API key set).
    // Auth failures are logged because log_mcp_requests wraps the admin-auth layer.
    let app = axum::Router::new()
        .route(HEALTH_PATH, axum::routing::get(health_handler))
        .route(HEALTHZ_PATH, axum::routing::get(healthz_handler))
        .route(STATUS_PATH, axum::routing::get(status_handler))
        // /remotes is a status-like read-only observability endpoint (lists the
        // configured federation peers). It is NOT in require_admin_auth's
        // `is_management` set, so it inherits exactly the same auth policy as
        // /status, /repos/:alias/info and /repos/:alias/doctor: reachable
        // without the admin key on localhost, protected by
        // require_auth_for_network on network binds. See REMOTES_PATH doc.
        .route(REMOTES_PATH, axum::routing::get(remotes_handler))
        .route("/repos", axum::routing::post(add_repo_handler))
        .route("/repos/:alias", axum::routing::delete(remove_repo_handler))
        .route("/reload", axum::routing::post(reload_handler))
        .route(
            "/repos/:alias/reindex",
            axum::routing::post(reindex_handler),
        )
        .route("/repos/:alias/info", axum::routing::get(info_handler))
        // /doctor is a POST but is intentionally read-only (diagnostics only, no
        // --fix path), so like /info and /status it is NOT in require_admin_auth's
        // management set — reachable without the admin key on localhost, and still
        // protected by require_auth_for_network on network binds. If doctor ever
        // gains a mutating mode, add it to `is_management` in require_admin_auth.
        .route("/repos/:alias/doctor", axum::routing::post(doctor_handler))
        // REST endpoints — federation-friendly HTTP+JSON mirror of the read-only
        // MCP tools (search/find/explore/get_chunk). Lets a remote codesearch
        // serve be queried WITHOUT an MCP session. Same auth layers as /mcp &
        // /status (require_auth_for_network on network binds). Read-only by
        // construction (the underlying tools never mutate the index).
        .route(
            SEARCH_PATH,
            axum::routing::post(crate::mcp::rest_search_handler),
        )
        .route(
            FIND_PATH,
            axum::routing::post(crate::mcp::rest_find_handler),
        )
        .route(
            EXPLORE_PATH,
            axum::routing::post(crate::mcp::rest_explore_handler),
        )
        .route(
            CHUNK_PATH,
            axum::routing::get(crate::mcp::rest_get_chunk_handler),
        )
        .nest_service(MCP_ENDPOINT_PATH, mcp_service)
        .layer(axum::middleware::from_fn(require_admin_auth))
        .layer(axum::middleware::from_fn(log_mcp_requests))
        .layer(axum::middleware::from_fn(require_auth_for_network))
        .layer(axum::Extension(network_auth))
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

    // Write serve_url to ~/.codesearch/serve_url so git hooks can find us
    if let Ok(dir) = config_dir() {
        let url_file = dir.join("serve_url");
        if let Err(e) = std::fs::write(&url_file, &serve_url) {
            warn!("Failed to write serve_url file: {}", e);
        }
    }

    let tui_cancel = cancel_token.clone();
    let tui_state = serve_state.clone();
    let tui_url = serve_url.clone();

    let tui_handle = if !no_tui {
        tui::maybe_spawn_tui(tui_state, tui_cancel, tui_url)
    } else {
        None
    };

    // ── Startup phase orchestration ──
    // Phase 1 warms all repos sequentially (text/vector ready).
    // Phase 2 runs gated C# SCIP rebuilds ordered by last_changed.
    {
        let phase_state = serve_state.clone();
        tokio::spawn(async move {
            // reconcile_all_paths spawns git subprocesses and traverses the
            // filesystem while holding the config RwLock write-guard.  Running
            // it on a Tokio worker thread would starve the async runtime and
            // block all concurrent config.read() calls for the entire duration.
            // spawn_blocking offloads the synchronous work to the blocking
            // thread pool, then we await the handle before proceeding to Phase 1.
            let reconcile_state = phase_state.clone();
            if let Err(e) =
                tokio::task::spawn_blocking(move || reconcile_state.reconcile_all_paths()).await
            {
                warn!("reconcile: spawn_blocking panicked: {:?}", e);
            }
            phase_state.run_phase_1_warmup_all().await;
            phase_state.run_phase_2_csharp_scip().await;
            phase_state.run_phase_3_prewarm().await;
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

    // ── Cloud keep-warm (scale-to-zero suspend after idle) ──
    // On a scale-to-zero host (e.g. Azure Container Apps) no ingress traffic
    // means the platform suspends the replica. While the most recent real tool
    // call is younger than the idle window, self-ping our own ingress FQDN to
    // generate traffic and stay warm; once idle exceeds the window, stop and let
    // the host suspend. The next real request wakes us automatically.
    let keep_warm_url = keep_warm_url
        .filter(|u| !u.is_empty())
        .or_else(|| std::env::var(crate::constants::KEEP_WARM_URL_ENV).ok())
        .filter(|u| !u.is_empty());
    if let Some(base_url) = keep_warm_url {
        let idle_suspend = idle_suspend_secs
            .or_else(|| {
                std::env::var(crate::constants::IDLE_SUSPEND_SECS_ENV)
                    .ok()
                    .and_then(|s| s.parse().ok())
            })
            .unwrap_or(crate::constants::DEFAULT_IDLE_SUSPEND_SECS);
        let ping_url = format!("{}{}", base_url.trim_end_matches('/'), HEALTHZ_PATH);
        let kw_state = serve_state.clone();
        let kw_cancel = cancel_token.clone();
        let start = Instant::now();
        info!(
            "🔥 keep-warm enabled: pinging {} every {}s while idle < {}s",
            ping_url,
            crate::constants::KEEP_WARM_INTERVAL_SECS,
            idle_suspend
        );
        tokio::spawn(async move {
            let interval =
                std::time::Duration::from_secs(crate::constants::KEEP_WARM_INTERVAL_SECS);
            let client = reqwest::Client::new();
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {
                        // Fall back to the server start time when no query has
                        // happened yet, so a freshly deployed replica stays warm
                        // for the full idle window before first use.
                        let last = kw_state.most_recent_tool_call().unwrap_or(start);
                        if last.elapsed().as_secs() < idle_suspend {
                            let _ = client
                                .get(&ping_url)
                                .timeout(std::time::Duration::from_secs(10))
                                .send()
                                .await;
                        }
                    }
                    _ = kw_cancel.cancelled() => break,
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

    // Clean up serve_url file on shutdown
    if let Ok(dir) = config_dir() {
        let url_file = dir.join("serve_url");
        let _ = std::fs::remove_file(&url_file);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_api_key_matches() {
        assert!(api_key_matches("secret-key", "secret-key"));
        assert!(!api_key_matches("secret-key", "secret-keX"));
        assert!(!api_key_matches("secret", "secret-key")); // different length
        assert!(!api_key_matches("", "secret-key"));
        assert!(api_key_matches("", "")); // both empty digests are equal
                                          // Case-sensitive and exact.
        assert!(!api_key_matches("Secret-Key", "secret-key"));
    }

    #[test]
    fn rest_service_drop_does_not_touch_active_sessions() {
        // Per-request REST services (built via make_service for /search /find
        // /explore /chunk, NOT the serve MCP session factory) must never touch
        // active_sessions: their Drop must NOT decrement the counter, or it
        // underflows to u64::MAX. Regression guard for the tracks_session fix.
        let state = std::sync::Arc::new(ServeState::new(ReposConfig::default(), None));
        {
            let _svc = crate::mcp::CodesearchService::new_for_serve(state.clone()).unwrap();
        }
        assert_eq!(
            state.active_session_count(),
            0,
            "REST service drop underflowed active_sessions"
        );
    }

    #[test]
    fn tracked_session_drop_balances_active_sessions() {
        // A genuine MCP session increments on connect and the serve factory
        // marks it tracked, so Drop decrements and the counter returns to 0.
        let state = std::sync::Arc::new(ServeState::new(ReposConfig::default(), None));
        let _id = state.session_connected();
        {
            let mut svc = crate::mcp::CodesearchService::new_for_serve(state.clone()).unwrap();
            svc.mark_session_tracked();
        }
        assert_eq!(
            state.active_session_count(),
            0,
            "tracked session did not balance"
        );
    }

    #[tokio::test]
    async fn await_fsw_shutdown_joins_exited_task_and_removes_entry() {
        // `await_fsw_shutdown` must (a) remove the alias from `fsw_tasks` and
        // (b) actually await (join) the task to completion — not just drop the
        // handle. We prove the join happened by observing a side-effect the
        // task sets on exit. Regression guard for the Windows DB-delete fix:
        // if someone removes the join, the LMDB env stays open and the task's
        // Arc<SharedStores> clone keeps the mmap handle locked on Windows.
        let state = ServeState::new(ReposConfig::default(), None);
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done_clone = done.clone();
        let handle = tokio::spawn(async move {
            // Yield once so the task isn't already-finished at insert time.
            tokio::task::yield_now().await;
            done_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        state.fsw_tasks.insert("repo-x".to_string(), handle);
        state.await_fsw_shutdown("repo-x").await;
        assert!(
            !state.fsw_tasks.contains_key("repo-x"),
            "fsw_tasks entry not removed"
        );
        assert!(
            done.load(std::sync::atomic::Ordering::SeqCst),
            "FSW task was not joined to completion"
        );
    }

    #[tokio::test]
    async fn await_fsw_shutdown_noop_on_missing_alias() {
        // A repo that never had an FSW task (Warm/Readonly/Conflicted) must
        // not panic — the map lookup is the no-op guard.
        let state = ServeState::new(ReposConfig::default(), None);
        state.await_fsw_shutdown("never-spawned").await;
        assert!(state.fsw_tasks.is_empty());
    }

    /// Regression guard: `GET /remotes` must NEVER expose a peer's `api_key`.
    ///
    /// `RemotePeerInfo` is a dedicated projection struct with no `api_key`
    /// field — serde cannot serialize a field that doesn't exist, so the
    /// shared secret cannot leak even by accident. This test locks that
    /// defense-in-depth: if a future change adds an `api_key` field to
    /// `RemotePeerInfo` (or otherwise lets the key into the response shape),
    /// this assertion fails.
    #[test]
    fn remote_peer_info_never_serializes_api_key() {
        use crate::db_discovery::repos::RemotePeer;

        // Build a peer carrying a real-looking secret, exactly as it lives in
        // repos.json, then project it the same way `remotes_handler` does.
        let peer = RemotePeer {
            url: "https://codesearch-serve.example.internal".to_string(),
            api_key: "supersecret-LEAK-MARKER-do-not-serialize".to_string(),
            group: Some("all".to_string()),
            timeout_secs: Some(90),
        };
        let info = RemotePeerInfo {
            alias: "cloud".to_string(),
            url: peer.url.clone(),
            group: peer.group.clone(),
            timeout_secs: peer.timeout_secs,
        };

        let json = serde_json::to_string(&info).expect("RemotePeerInfo must serialize");

        // The four whitelisted fields are present:
        assert!(json.contains("cloud"), "alias missing: {json}");
        assert!(
            json.contains("codesearch-serve.example.internal"),
            "url missing: {json}"
        );
        assert!(json.contains("all"), "group missing: {json}");
        assert!(json.contains("90"), "timeout_secs missing: {json}");

        // The secret is NOT present — neither the field name nor the value:
        assert!(
            !json.contains("api_key"),
            "api_key FIELD leaked into /remotes response shape: {json}"
        );
        assert!(
            !json.contains("supersecret-LEAK-MARKER"),
            "api_key VALUE leaked into /remotes response: {json}"
        );
    }

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

    // ------------------------------------------------------------------
    // Central store-creation / register path — regression guards.
    //
    // This is the point that has silently broken multiple times: opening or
    // creating a repo's database for a BRAND-NEW repo whose `.codesearch.db`
    // directory does not exist yet. The failure mode was a misleading
    // "Database is locked by another process" error -> HTTP 500 on POST /repos
    // -> repos.json registration rolled back -> CLI fell back to a local
    // duplicate index (control never handed to serve).
    //
    // RULE FOR THESE TESTS: never pre-create the `.codesearch.db` directory.
    // Earlier tests masked this exact bug by creating it first. The create /
    // register path must be exercised with the directory genuinely absent.
    // ------------------------------------------------------------------

    /// Core invariant: `try_open_stores(allow_create = true)` on a repo whose
    /// database directory does not exist yet MUST create it and return a
    /// writable handle — never a "locked"/open error. This is the single
    /// assertion that directly catches the regression class.
    #[tokio::test]
    async fn try_open_stores_creates_db_for_brand_new_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("brandnew");
        std::fs::create_dir(&repo_path).unwrap();
        let db_path = repo_path.join(DB_DIR_NAME);
        assert!(
            !db_path.exists(),
            "test precondition violated: db dir must NOT be pre-created"
        );

        let state = state_with_config(ReposConfig::default());

        match state.try_open_stores("brandnew", &db_path, true) {
            Ok(OpenedStores::Write(_)) => {}
            Ok(OpenedStores::Readonly(_)) => {
                panic!("brand-new repo opened Readonly; expected Write")
            }
            Err(e) => panic!(
                "opening stores for a brand-new repo (allow_create=true) must succeed, got: {e}"
            ),
        }

        assert!(
            db_path.exists(),
            "the .codesearch.db directory should have been created"
        );
    }

    /// End-to-end guard for the exact symptom pair: `POST /repos` for a repo
    /// whose database does not exist yet must return 202 Accepted, persist the
    /// alias to repos.json, and register the repo in WRITE mode — it must NOT
    /// return 500 and roll back the registration.
    ///
    /// Determinism: `#[tokio::test]` uses a current-thread runtime, so the
    /// background reindex task spawned by the handler cannot preempt this test
    /// (no `.await` follows the handler call). All assertions observe the
    /// handler's synchronous pre-spawn state — no embedding model required, no
    /// race. `persist_config` honors the temp config override, so the real
    /// `~/.codesearch/repos.json` is never touched.
    #[tokio::test]
    async fn add_repo_handler_registers_brand_new_repo_without_rollback() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = tmp.path().join("brandnew");
        std::fs::create_dir(&repo_path).unwrap();
        let db_path = repo_path.join(DB_DIR_NAME);
        assert!(!db_path.exists(), "precondition: db dir must not exist yet");

        let state = Arc::new(state_with_config(ReposConfig::default()));

        let (status, body) = add_repo_handler(
            axum::extract::State(state.clone()),
            axum::extract::Json(AddRepoRequest {
                path: repo_path.clone(),
                alias: Some("brandnew".to_string()),
                model: None,
            }),
        )
        .await;

        assert_eq!(
            status,
            axum::http::StatusCode::ACCEPTED,
            "brand-new repo register must be accepted (not 500), got {}: {}",
            status,
            body.0
        );

        // Registration persisted, NOT rolled back.
        assert!(
            state.config_snapshot().repos.contains_key("brandnew"),
            "alias must remain in repos.json after register (no rollback)"
        );

        // Registered in memory as Write so the fast-path avoids a second open.
        assert_eq!(
            state.repo_lock_status("brandnew"),
            Some("write"),
            "repo should be registered as Write immediately after add"
        );

        assert!(
            db_path.exists(),
            "the .codesearch.db directory should have been created"
        );
    }

    /// `persist_config` must write to the override path (and therefore be
    /// observable by `reload_if_changed`/`config_snapshot`) rather than the real
    /// `~/.codesearch/repos.json`. Guards the wiring that makes the register
    /// path hermetically testable.
    #[test]
    fn persist_config_honors_override_path() {
        let tmp = tempfile::tempdir().unwrap();
        let config_file = tmp.path().join("repos.json");
        let repo_path = tmp.path().join("somerepo");
        std::fs::create_dir(&repo_path).unwrap();

        ReposConfig::default().save_to(&config_file).unwrap();
        let state = ServeState::new(ReposConfig::default(), Some(config_file.clone()));

        {
            let mut cfg = state.config.write().unwrap();
            cfg.register_with_alias(repo_path.clone(), Some("somerepo".to_string()))
                .unwrap();
            state.persist_config(&cfg).unwrap();
        }

        // The override file on disk must contain the alias.
        let on_disk = ReposConfig::load_from(&config_file).unwrap();
        assert!(
            on_disk.repos.contains_key("somerepo"),
            "persist_config must write to the override path"
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

    /// `/healthz` is exempt from `require_auth_for_network`: reachable without a
    /// key even on a (simulated) network bind, while `/health` stays protected.
    #[tokio::test]
    async fn healthz_is_unauthenticated_on_network_bind() {
        let network_auth = NetworkAuthConfig {
            is_network_bind: true,
            api_key: Some("secret-key".to_string()),
        };

        let app = axum::Router::new()
            .route(HEALTH_PATH, axum::routing::get(health_handler))
            .route(HEALTHZ_PATH, axum::routing::get(healthz_handler))
            .layer(axum::middleware::from_fn(require_auth_for_network))
            .layer(axum::Extension(network_auth));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let client = reqwest::Client::new();

        // /healthz reachable WITHOUT a key on a network bind.
        let resp = client
            .get(format!("http://{}/healthz", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "/healthz must be public on a network bind"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(
            body.get("status").and_then(|v| v.as_str()),
            Some("ok"),
            "/healthz body must be {{\"status\":\"ok\"}}"
        );

        // /health stays protected on a network bind (401 without a key).
        let resp = client
            .get(format!("http://{}/health", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "/health must still require auth on a network bind"
        );
    }

    /// Verify that the /repos/:alias/info and /repos/:alias/doctor routes are
    /// registered and reachable. Starts a real axum server on a random port and
    /// asserts that an unknown alias yields our handler's 404 (not axum's 404).
    #[tokio::test]
    async fn info_doctor_routes_registered() {
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
            .route("/repos/:alias/info", axum::routing::get(info_handler))
            .route("/repos/:alias/doctor", axum::routing::post(doctor_handler))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // Give the server a moment to start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let client = reqwest::Client::new();

        // GET unknown alias info → 404 from our handler (not axum's built-in 404)
        let resp = client
            .get(format!("http://{}/repos/unknown/info", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::NOT_FOUND,
            "expected 404 from info handler"
        );
        let body: serde_json::Value = resp
            .json()
            .await
            .expect("info handler should return JSON body for 404");
        assert!(
            body.get("error").is_some(),
            "expected JSON error body from info handler, got: {}",
            body
        );

        // POST unknown alias doctor → 404 from our handler (not axum's built-in 404)
        let resp = client
            .post(format!("http://{}/repos/unknown/doctor", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::NOT_FOUND,
            "expected 404 from doctor handler"
        );
        let body: serde_json::Value = resp
            .json()
            .await
            .expect("doctor handler should return JSON body for 404");
        assert!(
            body.get("error").is_some(),
            "expected JSON error body from doctor handler, got: {}",
            body
        );
    }

    /// Verify that the federation REST endpoints (/search, /find, /explore,
    /// /chunk/:id) are registered and reachable. Each must dispatch to OUR
    /// handler (returning a JSON body) rather than axum's built-in empty 404.
    /// Starts a real axum server on a random port.
    #[tokio::test]
    async fn rest_routes_are_registered() {
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
                crate::constants::SEARCH_PATH,
                axum::routing::post(crate::mcp::rest_search_handler),
            )
            .route(
                crate::constants::FIND_PATH,
                axum::routing::post(crate::mcp::rest_find_handler),
            )
            .route(
                crate::constants::EXPLORE_PATH,
                axum::routing::post(crate::mcp::rest_explore_handler),
            )
            .route(
                crate::constants::CHUNK_PATH,
                axum::routing::get(crate::mcp::rest_get_chunk_handler),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let client = reqwest::Client::new();

        // Helper: a response from OUR handler is either 200 (success) or 500
        // (McpError mapped), but ALWAYS a parseable JSON body — never axum's
        // built-in empty 404. The repo has no index, so the tools return
        // error/scope JSON; we only assert the route + handler are wired.
        async fn assert_our_handler(client: &reqwest::Client, url: String) -> serde_json::Value {
            let resp = client.get(&url).send().await.unwrap();
            // GET endpoints: must reach our handler (JSON body), status 200/500.
            assert!(
                resp.status() == reqwest::StatusCode::OK
                    || resp.status() == reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                "GET {} -> unexpected status {} (route not registered?)",
                url,
                resp.status()
            );
            resp.json().await.unwrap_or_else(|e| {
                panic!(
                    "GET {} did not return a JSON body from our handler: {}",
                    url, e
                )
            })
        }

        // POST /search — dispatches to rest_search_handler.
        let resp = client
            .post(format!("http://{}/search", addr))
            .json(&serde_json::json!({"query": "foo", "project": "testalias"}))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status() == reqwest::StatusCode::OK
                || resp.status() == reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "POST /search -> unexpected status {} (route not registered?)",
            resp.status()
        );
        let _body: serde_json::Value = resp
            .json()
            .await
            .expect("POST /search should return JSON from our handler, not axum's 404");

        // POST /find — dispatches to rest_find_handler.
        let resp = client
            .post(format!("http://{}/find", addr))
            .json(
                &serde_json::json!({"kind": "definition", "symbol": "foo", "project": "testalias"}),
            )
            .send()
            .await
            .unwrap();
        assert!(
            resp.status() == reqwest::StatusCode::OK
                || resp.status() == reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "POST /find -> unexpected status {} (route not registered?)",
            resp.status()
        );
        let _: serde_json::Value = resp
            .json()
            .await
            .expect("POST /find should return JSON from our handler");

        // POST /explore — dispatches to rest_explore_handler.
        let resp = client
            .post(format!("http://{}/explore", addr))
            .json(&serde_json::json!({"kind": "outline", "target": "somefile", "project": "testalias"}))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status() == reqwest::StatusCode::OK
                || resp.status() == reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "POST /explore -> unexpected status {} (route not registered?)",
            resp.status()
        );
        let _: serde_json::Value = resp
            .json()
            .await
            .expect("POST /explore should return JSON from our handler");

        // GET /chunk/1 — dispatches to rest_get_chunk_handler.
        let _ = assert_our_handler(
            &client,
            format!("http://{}/chunk/1?project=testalias", addr),
        )
        .await;
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

    /// Unit tests for `validate_path_within_allowed_roots`.
    ///
    /// These tests temporarily set/remove the `CODESEARCH_ALLOWED_ROOTS` env var.
    /// A static Mutex serializes env mutation to prevent races under parallel test execution.
    #[cfg(test)]
    mod allowed_roots_tests {
        use super::*;
        use std::path::PathBuf;
        use std::sync::Mutex;

        /// Global lock to serialize env var mutations across parallel test threads.
        static ENV_LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();

        fn lock() -> std::sync::MutexGuard<'static, ()> {
            ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
        }

        /// Helper: create a unique temp dir per test, return its canonical path.
        fn temp_root(suffix: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("codesearch_test_roots_{}", suffix));
            let _ = std::fs::create_dir_all(&dir);
            safe_canonicalize(&dir).unwrap()
        }

        fn clear_env() {
            std::env::remove_var(ALLOWED_ROOTS_ENV);
        }

        fn set_env(val: &str) {
            std::env::set_var(ALLOWED_ROOTS_ENV, val);
        }

        #[test]
        fn env_unset_allows_all() {
            let _guard = lock();
            clear_env();
            let path = PathBuf::from("/some/random/path");
            assert!(validate_path_within_allowed_roots(&path).is_ok());
        }

        #[test]
        fn env_empty_allows_all() {
            let _guard = lock();
            set_env("");
            let path = PathBuf::from("/some/random/path");
            assert!(validate_path_within_allowed_roots(&path).is_ok());
            clear_env();
        }

        #[test]
        fn path_within_root_is_allowed() {
            let _guard = lock();
            let root = temp_root("within");
            set_env(&root.display().to_string());
            let child = root.join("my-project");
            let _ = std::fs::create_dir_all(&child);
            let canonical_child = safe_canonicalize(&child).unwrap();
            assert!(validate_path_within_allowed_roots(&canonical_child).is_ok());
            clear_env();
        }

        #[test]
        fn exact_root_match_is_allowed() {
            let _guard = lock();
            let root = temp_root("exact");
            set_env(&root.display().to_string());
            assert!(validate_path_within_allowed_roots(&root).is_ok());
            clear_env();
        }

        #[test]
        fn path_outside_root_is_rejected() {
            let _guard = lock();
            let root = temp_root("outside");
            set_env(&root.display().to_string());
            // Construct a path guaranteed outside the temp root
            let outside = if cfg!(windows) {
                PathBuf::from("C:\\Windows\\System32")
            } else {
                PathBuf::from("/etc")
            };
            assert!(
                !outside.starts_with(&root),
                "Test setup error: outside path '{}' must not overlap root '{}'",
                outside.display(),
                root.display()
            );
            let result = validate_path_within_allowed_roots(&outside);
            assert!(result.is_err(), "Expected rejection for path outside root");
            assert!(result.unwrap_err().contains("outside allowed roots"));
            clear_env();
        }

        #[test]
        fn all_nonexistent_roots_rejects() {
            let _guard = lock();
            set_env("/nonexistent/path/abc;/also/nonexistent/xyz");
            let some_path = std::env::temp_dir();
            let canonical = safe_canonicalize(&some_path).unwrap();
            let result = validate_path_within_allowed_roots(&canonical);
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("No valid roots found"));
            clear_env();
        }

        #[test]
        fn semicolons_with_empty_segments_works() {
            let _guard = lock();
            let root = temp_root("semicolons");
            set_env(&format!(";{};;", root.display()));
            let child = root.join("project");
            let _ = std::fs::create_dir_all(&child);
            let canonical_child = safe_canonicalize(&child).unwrap();
            assert!(validate_path_within_allowed_roots(&canonical_child).is_ok());
            clear_env();
        }

        #[test]
        fn multiple_roots_any_match() {
            let _guard = lock();
            let root1 = temp_root("multi1");
            let root2 = temp_root("multi2");

            set_env(&format!("{};{}", root1.display(), root2.display()));

            // Path under root1
            let child1 = root1.join("project");
            let _ = std::fs::create_dir_all(&child1);
            let canonical1 = safe_canonicalize(&child1).unwrap();
            assert!(validate_path_within_allowed_roots(&canonical1).is_ok());

            // Path under root2
            let child2 = root2.join("project");
            let _ = std::fs::create_dir_all(&child2);
            let canonical2 = safe_canonicalize(&child2).unwrap();
            assert!(validate_path_within_allowed_roots(&canonical2).is_ok());

            clear_env();
        }
    }

    /// The reserved virtual "all" group must resolve to every registered alias
    /// via the serve-layer entry point used by MCP tools (issue #131).
    #[test]
    fn resolve_group_aliases_all_returns_every_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_a = tmp.path().join("repo-a");
        let repo_b = tmp.path().join("repo-b");
        std::fs::create_dir(&repo_a).unwrap();
        std::fs::create_dir(&repo_b).unwrap();

        let mut config = ReposConfig::default();
        config
            .register_with_alias(repo_a, Some("alpha".to_string()))
            .unwrap();
        config
            .register_with_alias(repo_b, Some("beta".to_string()))
            .unwrap();

        let state = state_with_config(config);

        let aliases = state
            .resolve_group_aliases(crate::constants::ALL_GROUP_NAME)
            .expect("'all' should resolve");
        assert_eq!(aliases, vec!["alpha".to_string(), "beta".to_string()]);

        // "all" is never stored — an unknown real group still errors.
        assert!(state.resolve_group_aliases("does-not-exist").is_err());
    }
}
