//! Central constants for codesearch configuration
//!
//! All string literals for paths, filenames, and configuration should be defined here
//! to avoid duplication and ensure consistency across the codebase.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// Global shutdown flag, set by the CTRL-C handler.
///
/// This uses a raw `AtomicBool` instead of relying solely on `CancellationToken`
/// because the indexing pipeline is largely synchronous (ONNX inference, file I/O)
/// and the flag must be visible from any thread without async polling.
///
/// Checked between files and between embedding mini-batches so that CTRL-C
/// is honoured within a few seconds even during heavy CPU work.
pub static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Check whether a graceful shutdown has been requested (CTRL-C).
#[inline]
pub fn is_shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
}

/// Check whether a graceful shutdown has been requested via either
/// the global AtomicBool (OS signal) or a CancellationToken.
///
/// This helper consolidates the two shutdown mechanisms used throughout the codebase
/// to reduce duplication and improve maintainability.
#[inline]
pub fn check_shutdown(cancel_token: &tokio_util::sync::CancellationToken) -> bool {
    is_shutdown_requested() || cancel_token.is_cancelled()
}

/// Name of the database directory in project roots
pub const DB_DIR_NAME: &str = ".codesearch.db";

/// Name of the global config directory in user home
pub const CONFIG_DIR_NAME: &str = ".codesearch";

/// Name of the file metadata database
pub const FILE_META_DB_NAME: &str = "file_meta.json";

/// Subdirectory name for embedding models within the global config dir
const MODELS_SUBDIR: &str = "models";

/// Log directory name within .codesearch.db
pub const LOG_DIR_NAME: &str = "logs";

/// Default log file name
pub const LOG_FILE_NAME: &str = "codesearch.log";

/// Serve-specific log file name (written to ~/.codesearch/logs/)
pub const SERVE_LOG_FILE_NAME: &str = "serve.log";

/// Default number of log files to retain
pub const DEFAULT_LOG_MAX_FILES: usize = 5;

/// Default log retention period in days
pub const DEFAULT_LOG_RETENTION_DAYS: u64 = 5;

/// Get the global models cache directory (~/.codesearch/models/).
///
/// This centralizes embedding model downloads so they are shared across all
/// databases instead of being duplicated per-project. The directory is created
/// if it does not exist.
///
/// Falls back to a temp directory if the home directory cannot be determined.
pub fn get_global_models_cache_dir() -> anyhow::Result<PathBuf> {
    let base =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;

    let models_dir = base.join(CONFIG_DIR_NAME).join(MODELS_SUBDIR);

    if !models_dir.exists() {
        std::fs::create_dir_all(&models_dir).map_err(|e| {
            anyhow::anyhow!(
                "Failed to create global models cache directory {}: {}",
                models_dir.display(),
                e
            )
        })?;
    }

    Ok(models_dir)
}

/// Get the global cache directory (~/.codesearch/).
///
/// Used for client/auto mode logging when no local DB is available.
/// The directory is created if it does not exist.
pub fn get_global_cache_dir() -> PathBuf {
    let base = dirs::home_dir().unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let cache_dir = base.join(CONFIG_DIR_NAME);
    if !cache_dir.exists() {
        let _ = std::fs::create_dir_all(&cache_dir);
    }
    cache_dir
}

/// Name of the global codesearchignore file in ~/.codesearch/
pub const GLOBAL_CODESEARCHIGNORE_FILE: &str = ".codesearchignore";

/// Get the path to the global .codesearchignore file (~/.codesearch/.codesearchignore).
///
/// This file uses the same gitignore syntax and is applied to all indexed repos,
/// providing a way to set ignore rules without modifying repo-local files.
/// Repo-local `.codesearchignore` and `.gitignore` take precedence.
///
/// Returns `None` only if the home directory cannot be determined.
pub fn global_codesearchignore_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| {
        home.join(CONFIG_DIR_NAME)
            .join(GLOBAL_CODESEARCHIGNORE_FILE)
    })
}

/// Name of the global extension→language map file in ~/.codesearch/
pub const GLOBAL_EXTENSION_MAP_FILE: &str = "extensions.json";

/// Env var overriding the location of the extension-map file.
///
/// Mainly for tests and power users who keep config outside `~/.codesearch/`.
pub const EXTENSION_MAP_ENV: &str = "CODESEARCH_EXTENSION_MAP";

/// Get the path to the global extension→language map.
///
/// This is a small JSON object mapping a file extension (with or without the
/// leading dot) to a language name, e.g. `{ "inc": "php", "h": "cpp" }`. It is
/// applied to every indexed repo and lets users teach codesearch about
/// non-standard extensions (or deliberately remap known ones) without touching
/// the binary. User overrides take precedence over the built-in extension table.
///
/// The path resolves to `$CODESEARCH_EXTENSION_MAP` when set and non-empty,
/// otherwise `~/.codesearch/extensions.json`. Returns `None` only when neither
/// the env var nor the home directory is available.
pub fn global_extension_map_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var(EXTENSION_MAP_ENV) {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    dirs::home_dir().map(|home| home.join(CONFIG_DIR_NAME).join(GLOBAL_EXTENSION_MAP_FILE))
}

/// Name of the repos configuration file
pub const REPOS_CONFIG_FILE: &str = "repos.json";

/// Reserved name of the virtual group that resolves to ALL registered repos.
///
/// This group is NOT stored in `repos.json` — it is resolved dynamically by
/// `ServeState::resolve_group_aliases` and `ReposConfig::resolve_group`, so it
/// always reflects the current set of registered aliases. Because it is reserved,
/// `codesearch groups add all` is rejected. See issue #131.
pub const ALL_GROUP_NAME: &str = "all";

/// Default LMDB map size in megabytes (1024MB).
///
/// This is the maximum virtual address space reserved for the memory-mapped database.
/// On Linux/macOS this is just an address space reservation (no physical RAM until data is written).
/// On Windows the file may be pre-allocated to this size, so keeping it small matters.
/// 1024MB is sufficient for most codebases (~200k chunks × ~5KB = ~1024MB).
/// Override with `CODESEARCH_LMDB_MAP_SIZE_MB` environment variable.
pub const DEFAULT_LMDB_MAP_SIZE_MB: usize = 1024;

/// Maximum LMDB map size in megabytes (16384MB = 16GB).
///
/// This is the hard upper limit for auto-resizing when MDB_MAP_FULL errors occur.
/// Prevents unbounded growth and potential disk exhaustion. On 64-bit Linux/macOS
/// the mapsize is only a virtual-address-space reservation (free until written),
/// so a high cap is safe; on Windows the LMDB file may be pre-allocated to the
/// current (grown) size, but growth only happens on demand when MDB_MAP_FULL
/// actually bites, so raising the ceiling does not change the steady-state size.
///
/// The previous 8GB cap was too low for very large corpora — e.g. a 1GB /
/// 53k-file cargo-registry source producing >1.2M chunks legitimately exceeds
/// it (GitHub issue #189). 16GB is ample headroom for monorepo-scale indexes
/// (the #189 repro needed just past 8GB; ~1.2M 384-dim quantized vectors +
/// arroy overhead ≈ 1.8GB raw), without risking disk exhaustion.
///
/// Override at runtime with `CODESEARCH_MAX_LMDB_MAP_SIZE_MB` (see
/// [`max_lmdb_map_size_mb`]); the override is clamped to at least
/// [`DEFAULT_LMDB_MAP_SIZE_MB`] — use it to raise the ceiling on extreme corpora
/// that need more than the 16GB default.
pub const MAX_LMDB_MAP_SIZE_MB: usize = 16384;

/// Resolve the effective maximum LMDB map size in MB for the current process.
///
/// Reads the `CODESEARCH_MAX_LMDB_MAP_SIZE_MB` env var if set (clamped to at
/// least [`DEFAULT_LMDB_MAP_SIZE_MB`]); otherwise falls back to the
/// [`MAX_LMDB_MAP_SIZE_MB`] compile-time default. This lets operators with
/// extreme corpora — or Windows instances that want a lower ceiling — tune the
/// auto-resize cap without rebuilding.
pub fn max_lmdb_map_size_mb() -> usize {
    std::env::var("CODESEARCH_MAX_LMDB_MAP_SIZE_MB")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .map(|v| v.max(DEFAULT_LMDB_MAP_SIZE_MB))
        .unwrap_or(MAX_LMDB_MAP_SIZE_MB)
}

#[allow(dead_code)]
/// Default maximum number of entries in persistent embedding cache.
///
/// The persistent embedding cache stores computed embeddings on disk keyed by
/// content hash (SHA256), allowing fast branch switches by reusing embeddings
/// across branches. Each entry is ~1.5KB (384 dims × 4 bytes), so:
/// - 200,000 entries ≈ 300MB on disk
/// - Sufficient for 10+ branches worth of embeddings
/// - Override with `CODESEARCH_EMBEDDING_CACHE_MAX_ENTRIES` environment variable.
pub const DEFAULT_EMBEDDING_CACHE_MAX_ENTRIES: usize = 200_000;

/// Default embedding cache memory limit in MB.
///
/// The embedding cache stores recently computed embeddings in memory (Moka LRU cache)
/// to avoid re-computing them during incremental indexing. This is real physical memory.
/// 100MB is sufficient since files are processed sequentially during indexing.
/// Override with `CODESEARCH_CACHE_MAX_MEMORY` environment variable.
pub const DEFAULT_CACHE_MAX_MEMORY_MB: usize = 100;

/// Default LMDB map size (in MB) for the **persistent** embedding cache
/// (`PersistentEmbeddingCache` at `~/.codesearch/embedding_cache/<model>/`).
///
/// Each cache entry is a SHA256 key + `Vec<f32>` of 384 dims ≈ 1.5 KB, so 512 MB
/// holds roughly 340k embeddings — enough for typical multi-branch use. The cache
/// auto-resizes (doubling, up to [`MAX_LMDB_MAP_SIZE_MB`]) on `MDB_MAP_FULL`, so
/// this is only the *starting* size: very large corpora (e.g. the >1.2M-chunk
/// cargo-registry repro from issue #189) will grow past it on demand.
///
/// Distinct from [`DEFAULT_LMDB_MAP_SIZE_MB`] (the *vector store* starting size,
/// 1024 MB) because the cache holds only `(hash → Vec<f32>)`, no arroy tree or
/// chunk metadata, so it is smaller per-entry.
pub const DEFAULT_EMBEDDING_CACHE_LMDB_MAP_SIZE_MB: usize = 512;

/// File watcher debounce time in milliseconds
pub const DEFAULT_FSW_DEBOUNCE_MS: u64 = 2000;

/// Lock file name to indicate an active writer instance
/// This prevents multiple processes from writing to the same database
pub const WRITER_LOCK_FILE: &str = ".writer.lock";

/// Default host for `codesearch serve` (MCP streamable HTTP mode).
/// Override with `--host` or `CODESEARCH_SERVE_HOST`.
/// Use `0.0.0.0` to bind on all interfaces (e.g. in Docker containers).
pub const DEFAULT_SERVE_HOST: &str = "127.0.0.1";

/// Environment variable to override the serve host.
pub const SERVE_HOST_ENV: &str = "CODESEARCH_SERVE_HOST";

/// Default port for `codesearch serve` (MCP streamable HTTP mode).
/// Override with `--port` or `CODESEARCH_SERVE_PORT`.
pub const DEFAULT_SERVE_PORT: u16 = 39725;

/// Environment variable to override the serve port.
pub const SERVE_PORT_ENV: &str = "CODESEARCH_SERVE_PORT";

/// Resolve the effective serve host from env or default.
/// Returns owned `String` because env vars are runtime values.
/// Used by CLI delegation, MCP client, and serve startup to construct URLs.
pub fn resolve_serve_host() -> String {
    std::env::var(SERVE_HOST_ENV)
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| DEFAULT_SERVE_HOST.to_string())
}

/// Environment variable to set the admin API key for management endpoints.
/// When set, all management routes (`POST /repos`, `DELETE /repos/:alias`,
/// `POST /repos/:alias/reindex`, `POST /reload`) require this key.
/// When unset or empty, management routes are unauthenticated (backward compatible).
/// The key is validated against `Authorization: Bearer <key>` or `X-API-Key: <key>` headers.
pub const SERVE_API_KEY_ENV: &str = "CODESEARCH_SERVE_API_KEY";

/// Environment variable to restrict which filesystem roots can be indexed.
/// Semicolon-separated list of canonical directory paths. When set, `POST /repos`
/// and `--register` paths must reside under one of the listed roots.
/// When unset or empty, all paths are allowed (backward compatible).
/// Example: `CODESEARCH_ALLOWED_ROOTS=/home/user/repos;/opt/code`
pub const ALLOWED_ROOTS_ENV: &str = "CODESEARCH_ALLOWED_ROOTS";

/// Environment variable to override the rmcp Streamable HTTP server's
/// `allowed_hosts` list (DNS-rebinding defence, GHSA-89vp-x53w-74fx).
///
/// rmcp's default is loopback-only (`["localhost", "127.0.0.1", "::1"]`),
/// which rejects the container hostname in containerised deployments with
/// `WARN ... rejected request with disallowed Host header`. Setting this
/// env var to a comma-separated list of hostnames / `host:port` replaces
/// the default allowlist.
///
/// When unset or empty, the rmcp default applies. See issue #149.
/// Example: `CODESEARCH_ALLOWED_HOSTS=codesearch.internal, codesearch:39725`
pub const ALLOWED_HOSTS_ENV: &str = "CODESEARCH_ALLOWED_HOSTS";

/// Environment variable to disable the rmcp Streamable HTTP server's
/// `Host` header validation entirely.
///
/// **Dangerous**: turns off DNS-rebinding protection (GHSA-89vp-x53w-74fx).
/// Only set when codesearch runs behind a reverse proxy (nginx, Caddy,
/// Traefik) that itself validates the `Host` header against an allowlist.
/// Any other value leaves validation enabled.
///
/// Accepts `1` or `true` (case-insensitive) to disable.
/// Example: `CODESEARCH_DISABLE_HOST_VALIDATION=1`
pub const DISABLE_HOST_VALIDATION_ENV: &str = "CODESEARCH_DISABLE_HOST_VALIDATION";

/// Default base URL for connecting to a local `codesearch serve` instance.
/// Used as the clap `--url` default and in `serve_base_url()`.
///
/// **Must stay in sync with `DEFAULT_SERVE_PORT`.**
/// A `#[test]` in this module asserts `DEFAULT_SERVE_URL` contains the port string
/// from `DEFAULT_SERVE_PORT`, so bumping one without the other will fail `cargo test`.
pub const DEFAULT_SERVE_URL: &str = "http://127.0.0.1:39725";

/// Management collection route served by `codesearch serve`:
/// `POST /repos { path, alias?, model? }` registers + indexes a new repo.
///
/// Per-repo routes are derived from this: `DELETE {REPOS_PATH}/:alias`,
/// `POST {REPOS_PATH}/:alias/reindex`, `GET {REPOS_PATH}/:alias/info`.
pub const REPOS_PATH: &str = "/repos";

/// Path prefix for the per-repo reindex HTTP API route.
/// Full path: `{REPOS_PATH}/{alias}{REPO_REINDEX_PATH_SUFFIX}`.
pub const REPO_REINDEX_PATH_PREFIX: &str = "/repos/";

/// Path suffix for the per-repo reindex HTTP API route.
pub const REPO_REINDEX_PATH_SUFFIX: &str = "/reindex";

/// Path suffix for the per-repo info HTTP API route.
/// Full path: `{REPOS_PATH}/{alias}{REPO_INFO_PATH_SUFFIX}`.
pub const REPO_INFO_PATH_SUFFIX: &str = "/info";

/// Health-check path served by `codesearch serve`.
pub const HEALTH_PATH: &str = "/health";

/// Unauthenticated liveness-probe path served by `codesearch serve`.
///
/// Unlike `HEALTH_PATH` (which reports the version and sits behind the
/// network-auth layer), this endpoint is ALWAYS reachable without an API key —
/// even on a non-localhost bind — and returns a fixed `{"status":"ok"}` body
/// with no version or repo information. Intended for container-orchestrator
/// liveness/readiness probes (e.g. Azure Container Apps) that cannot present
/// the Bearer key.
pub const HEALTHZ_PATH: &str = "/healthz";

/// MCP endpoint path served by `codesearch serve` (streamable HTTP).
pub const MCP_ENDPOINT_PATH: &str = "/mcp";

/// Status endpoint path served by `codesearch serve`.
/// Returns JSON snapshot of all repo states, sessions, and CPU usage.
pub const STATUS_PATH: &str = "/status";

/// Indexing-freshness endpoint path served by `codesearch serve`.
///
/// Cheap single-question probe for the grep-guard hook (and any caller that
/// needs to distinguish "no results" from "index mid-rebuild"): takes
/// `?path=<absolute path>`, resolves the containing registered repo, and
/// returns `{"covered":bool,"alias":..,"indexing":bool}` — `indexing` is true
/// while that repo has an active (non-stale) reindex in flight, which
/// includes the full refresh fired by a branch switch. Same auth class as
/// [`STATUS_PATH`]: reachable without the admin key on localhost, protected
/// by `require_auth_for_network` on network binds. `/healthz` remains the
/// ONLY always-unauthenticated endpoint — liveness and freshness are
/// different questions and stay on different paths.
pub const INDEXING_PATH: &str = "/indexing";

/// Remotes endpoint path served by `codesearch serve`.
///
/// Observability companion to [`STATUS_PATH`]: lists the configured federation
/// peers (the `remotes` map from `repos.json`) so an operator can see which
/// remotes this serve fans out to. Read-only and status-like — reachable
/// without the admin key on localhost, protected only by
/// `require_auth_for_network` on network binds (NOT in the `is_management`
/// set). **Never** exposes `api_key`: the handler projects each peer into a
/// dedicated `RemotePeerInfo` struct that structurally omits the secret.
pub const REMOTES_PATH: &str = "/remotes";

/// REST search endpoint (federation-friendly HTTP mirror of the `search` MCP
/// tool). POST a `SearchRequest` body; returns the tool's JSON payload.
pub const SEARCH_PATH: &str = "/search";

/// REST find endpoint (HTTP mirror of the `find` MCP tool).
/// POST a `FindRequest` body.
pub const FIND_PATH: &str = "/find";

/// REST explore endpoint (HTTP mirror of the `explore` MCP tool).
/// POST an `ExploreRequest` body.
pub const EXPLORE_PATH: &str = "/explore";

/// REST get-chunk endpoint (HTTP mirror of the `get_chunk` MCP tool).
/// GET `/chunk/:id?context_lines=&project=&group=`.
pub const CHUNK_PATH: &str = "/chunk/:id";

/// How long an open repo may remain idle (no queries) before it is evicted.
/// Eviction closes the DB handles, stops the FSW, and releases memory.
/// The repo is automatically re-opened on the next query.
/// Override with `CODESEARCH_REPO_IDLE_TIMEOUT_SECS`.
pub const REPO_IDLE_TIMEOUT_SECS: u64 = 30 * 60; // 30 minutes

/// How often the idle-reaper background task checks for repos to evict.
pub const REAPER_INTERVAL_SECS: u64 = 5 * 60; // 5 minutes

/// Environment variable to override the repo idle timeout.
pub const REPO_IDLE_TIMEOUT_ENV: &str = "CODESEARCH_REPO_IDLE_TIMEOUT_SECS";

// --- Cloud keep-warm (scale-to-zero suspend after idle) ----------------------

/// URL serve self-pings (its own ingress FQDN) to stay warm while active.
///
/// In a scale-to-zero host (e.g. Azure Container Apps), no ingress traffic →
/// the platform suspends the replica after its cooldown. While the most recent
/// real tool call is younger than `IDLE_SUSPEND_SECS_ENV`, serve periodically
/// GETs `<url>/healthz` to generate ingress traffic and stay warm. Once idle
/// exceeds that window it stops, letting the host suspend; the next real
/// request wakes it automatically. Empty/unset disables keep-warm.
///
/// Set via `--keep-warm-url` or this env var (flag takes precedence).
pub const KEEP_WARM_URL_ENV: &str = "CODESEARCH_KEEP_WARM_URL";

/// Environment variable to override the idle-before-suspend window.
pub const IDLE_SUSPEND_SECS_ENV: &str = "CODESEARCH_IDLE_SUSPEND_SECS";

/// Default idle window before serve stops self-pinging and lets the host
/// suspend the replica (2 hours).
pub const DEFAULT_IDLE_SUSPEND_SECS: u64 = 2 * 60 * 60;

/// How often the keep-warm task pings its own ingress while active.
pub const KEEP_WARM_INTERVAL_SECS: u64 = 2 * 60; // 2 minutes

// --- MCP proxy idle-disconnect (client side of scale-to-zero) -----------------

/// Environment variable to override how long the local `codesearch mcp` proxy
/// keeps its HTTP MCP session to the remote `codesearch serve` open while no
/// tool calls are flowing.
///
/// This is the client-side counterpart of `IDLE_SUSPEND_SECS_ENV`: a single
/// long-lived Streamable-HTTP session registers as a permanently open request at
/// the remote's ingress, so a scale-to-zero host (e.g. Azure Container Apps with
/// a KEDA HTTP scaler) never observes 0 concurrent requests and never suspends
/// the replica. Closing the session while idle lets it scale down; the next tool
/// call reconnects on demand.
pub const MCP_PROXY_IDLE_DISCONNECT_SECS_ENV: &str = "CODESEARCH_MCP_PROXY_IDLE_DISCONNECT_SECS";

/// Default idle window before the local MCP proxy closes its connection to the
/// remote serve hub (1 minute).
///
/// Deliberately short: it has to elapse *before* the host's own scale-in
/// cooldown can start, otherwise the replica never gets the chance to suspend
/// after real use stops. Still long enough that closely-spaced tool calls (an
/// agent issuing `search` → `get_chunk` → `find` in sequence) reuse one session
/// instead of thrashing connect/teardown.
///
/// `0` disables idle-disconnect entirely, restoring the previous behaviour of
/// one connection held open for the whole lifetime of the proxy process.
pub const DEFAULT_MCP_PROXY_IDLE_DISCONNECT_SECS: u64 = 60;

/// How often the MCP proxy's idle-checker task ticks. Bounds how long past the
/// configured window a connection may linger before being closed.
pub const MCP_PROXY_IDLE_CHECK_INTERVAL_SECS: u64 = 10;

/// Default per-peer federation request timeout (seconds) when a remote peer
/// does not specify its own `timeout_secs`. Shared by the federation client
/// and the `remote` CLI command so both report/apply the same default.
pub const DEFAULT_REMOTE_TIMEOUT_SECS: u64 = 15;

/// How long after a federated peer's `/status` refresh the embedded TUI still
/// considers that peer's activity "live" before reverting the activity column to
/// a stale `-`.
///
/// There is **no background `/status` poll of a federated peer at all**: a peer
/// is contacted only when a real tool call hits it (event-driven, see
/// `ServeState::record_remote_peer_activity`) or on an explicit operator
/// keypress (`i` info overlay). Outside of active use a mount's activity column
/// simply reads `-`, so this window only governs how long a *poked* value stays
/// visible before going stale again.
pub const REMOTE_ACTIVITY_FRESH_SECS: u64 = 5 * 60; // 5 minutes

/// Cadence of the embedded TUI's **config-only** mounted-remote row rebuild.
///
/// This tick issues NO HTTP to any peer: it re-reads the repos config (via
/// `ServeState::config_snapshot`) and rebuilds the mounted-remote rows so
/// mount/unmount edits and `l` reloads show up promptly. Because it never
/// contacts a peer it cannot wake a scale-to-zero replica, which is precisely
/// why it is safe to run on a short interval.
pub const REMOTE_ROW_REFRESH_SECS: u64 = 5;

/// Maximum wall-clock duration a single reindex may take before its
/// `active_reindexes` entry is considered **stale** (leaked).
///
/// Background indexing tasks (`reindex_handler`, `add_repo_handler`,
/// `spawn_force_reindex`, `trigger_symbol_rebuild`, and the file-watcher
/// branch-change callback) insert into `active_reindexes` at the start and
/// remove at the end.  Some of these tasks run inside fire-and-forget
/// `tokio::spawn` calls whose `JoinHandle` is discarded — a panic or
/// cancellation between insert and remove silently leaks the entry, leaving
/// the TUI stuck on "Indexing" forever.
///
/// Rather than chase every leak path, entries older than this threshold are
/// treated as absent by all read sites (`repo_statuses_lightweight`,
/// `evict_idle_repos`, and the reindex 409 guard) and lazily evicted. This
/// makes the system self-healing regardless of the leak cause.
///
/// 30 minutes is deliberately generous: a force reindex of a very large repo
/// or a full scip-csharp solution rebuild can legitimately take many minutes.
/// Any session stuck beyond that is almost certainly leaked.
/// Override with `CODESEARCH_MAX_INDEXING_SECS`.
pub const MAX_INDEXING_SECS: u64 = 30 * 60; // 30 minutes

/// Environment variable to override the maximum indexing duration.
pub const MAX_INDEXING_SECS_ENV: &str = "CODESEARCH_MAX_INDEXING_SECS";

/// Total number of attempts (initial request + retries) the federation client
/// makes against a remote peer that answers with a transient HTTP status
/// (502/503/504). Federated peers commonly run on scale-to-zero hosts (Azure
/// Container Apps): the first request after an idle period can hit a cold
/// start and surface as a 503 even though the peer is perfectly healthy. A
/// short bounded retry inside the active tool call absorbs most cold starts
/// before the caller ever sees them. This is NOT a poll: retries only happen
/// while a user-initiated tool call is already in flight against that peer
/// (the "never contact a federated peer on a cadence" rule in AGENTS.md is
/// about timers, and stays intact).
pub const REMOTE_PEER_RETRY_ATTEMPTS: u32 = 3;

/// Backoff (milliseconds) between federation retry attempts, one entry per
/// retry (so `REMOTE_PEER_RETRY_ATTEMPTS - 1` entries; the last entry is
/// reused if there are ever more retries than entries). Short by design —
/// the retry exists to catch a peer that is already warming, not to outwait
/// a long deployment. If the peer is still transient-failing after the last
/// attempt, the error message tells the caller to retry the same call in
/// ~30s instead of blocking the tool call longer.
pub const REMOTE_PEER_RETRY_BACKOFF_MS: &[u64] = &[3000, 8000];

/// Environment variable overriding every federation retry backoff with a
/// single millisecond value (test hook so retry tests don't sleep for real).
pub const REMOTE_PEER_RETRY_BACKOFF_ENV: &str = "CODESEARCH_REMOTE_RETRY_BACKOFF_MS";

/// Cooperative join window (seconds) for `await_index_task` and
/// `await_fsw_shutdown`: how long a background indexing / file-watcher task
/// is given to observe its `CancellationToken` and exit on its own before it
/// is force-aborted. Kept short so a stuck task cannot wedge `remove_repo`;
/// the follow-on DB-delete retry budget (`DB_DELETE_RETRY_BUDGET_SECS`) is
/// the outer bound for the whole shutdown.
pub const BG_TASK_COOPERATIVE_TIMEOUT_SECS: u64 = 5;

/// Total wall-clock budget (seconds) `remove_repo` spends retrying a locked
/// `.codesearch.db` delete after the background task is aborted. An indexing
/// task that ignores its token is force-aborted, but its `Arc<SharedStores>`
/// / LMDB handles are only released once the runtime finishes dropping the
/// aborted future; this budget covers that release window plus any OS
/// handle-close lag on Windows.
pub const DB_DELETE_RETRY_BUDGET_SECS: u64 = 60;

/// Initial backoff (milliseconds) for the locked-DB delete retry loop in
/// `remove_repo`; doubled each attempt up to `DB_DELETE_RETRY_BACKOFF_CAP_MS`.
pub const DB_DELETE_RETRY_INITIAL_MS: u64 = 200;

/// Upper bound (milliseconds) for the exponential backoff between locked-DB
/// delete retries in `remove_repo`.
pub const DB_DELETE_RETRY_BACKOFF_CAP_MS: u64 = 2000;

/// Poll interval (milliseconds) for the in-process LMDB-holder release wait
/// inside `remove_repo`'s locked-DB delete retry loop. After a lock-class
/// delete failure the loop polls `lmdb_registry::open_holders_under` at this
/// cadence until every in-process env under the DB dir is released (or
/// `DB_DELETE_RETRY_BUDGET_SECS` expires), so the next attempt runs against
/// an actually-unlocked directory instead of burning attempts blind.
pub const DB_DELETE_ENV_RELEASE_POLL_MS: u64 = 100;

/// Unallocated margin (seconds) the CLI's delegated `DELETE /repos/:alias`
/// request adds on top of serve's legitimate worst-case removal time —
/// `DB_DELETE_RETRY_BUDGET_SECS` plus one `BG_TASK_COOPERATIVE_TIMEOUT_SECS`
/// per cooperative join (FSW task + index task) — so the CLI receives
/// serve's honest locked-DB outcome (`db_deleted` / payload) instead of its
/// own request timeout firing first. The shared delegation client's 3 s
/// total timeout is fine for the `/health` probe but far shorter than a
/// legitimate slow removal (warmup cancellation + env-release wait +
/// retries); `try_delegate_rm_to_serve` builds the DELETE its own client
/// sized from these constants.
pub const RM_DELEGATE_DELETE_MARGIN_SECS: u64 = 10;

/// Default embedding dimensions used when metadata is missing or unreadable.
pub const DEFAULT_EMBEDDING_DIMENSIONS: usize = 384;

/// Environment variable to override repos config file path.
pub const REPOS_CONFIG_ENV: &str = "CODESEARCH_REPOS_CONFIG";

/// Environment variable to override how deep relocation scans for a moved repo.
pub const RELOCATE_MAX_DEPTH_ENV: &str = "CODESEARCH_RELOCATE_MAX_DEPTH";

/// Default bounded depth for the relocation scan (directories below the nearest
/// existing ancestor of a stale repo path).
pub const DEFAULT_RELOCATE_MAX_DEPTH: usize = 3;

/// Environment variable to set MCP mode: "auto", "client", or "local".
pub const MCP_MODE_ENV: &str = "CODESEARCH_MCP_MODE";

/// Timeout for serve health probe in auto/client mode (milliseconds).
pub const MCP_HEALTH_PROBE_TIMEOUT_MS: u64 = 500;

/// Environment variable to override the scip-csharp helper path.
pub const SCIP_CSHARP_HELPER_ENV: &str = "CODESEARCH_SCIP_CSHARP";

/// Helper binary name for the C# symbol indexer (without extension).
pub const SCIP_CSHARP_HELPER_NAME: &str = "scip-csharp";

/// Subdirectory within the codesearch install dir where language helpers live.
pub const HELPERS_SUBDIR: &str = "helpers";

/// Debounce time in milliseconds for .cs file changes triggering a symbol rebuild.
pub const SCIP_CSHARP_DEBOUNCE_MS: u64 = 60_000; // 60 seconds

/// LMDB database name for the SCIP symbols table.
pub const SCIP_SYMBOLS_DB_NAME: &str = "scip_symbols";

/// LMDB database name for the SCIP per-repo metadata table.
pub const SCIP_META_DB_NAME: &str = "scip_meta";

/// LMDB metadata key for the last rebuild timestamp.
pub const SCIP_REBUILD_TIMESTAMP_KEY: &str = "last_rebuild_ts";

/// LMDB metadata key for the git HEAD sha the symbol index was built for.
/// Written on rebuild when the repo HEAD is readable; absent means unknown
/// (never written, or git could not be read at build time).
pub const SCIP_HEAD_SHA_KEY: &str = "head_sha";

/// LMDB table mapping `(file:line)` positions to `[symbol_keys]`.
/// Used for O(1) position-based symbol lookup.
pub const SCIP_POSITION_DB_NAME: &str = "scip_positions";

/// LMDB table mapping simple names (last segment of SCIP symbol)
/// to `[full_symbol_keys]`. Used for O(1) fuzzy symbol lookup.
pub const SCIP_SIMPLE_NAMES_DB_NAME: &str = "scip_simple_names";

/// LMDB table caching on-demand reference results from `scip-csharp find-refs`.
/// Key: full SCIP symbol key. Value: `[v1, bincode(Vec<StoredReference>)]` (same
/// format as `scip_symbols`). Populated on first `find_impact` call for a symbol;
/// cleared when the definition index is rebuilt. Gives O(1) lookup on 2nd+ calls.
pub const SCIP_REF_CACHE_DB_NAME: &str = "scip_ref_cache";

/// Language identifier for the C# symbol indexer.
/// Used as a key in `SymbolIndexerRegistry` lookups and TUI status maps.
pub const LANG_CSHARP: &str = "csharp";

/// Language identifier for the TypeScript symbol indexer.
/// Used as a key in `SymbolIndexerRegistry` lookups and TUI status maps.
pub const LANG_TYPESCRIPT: &str = "typescript";

/// Environment variable override for the `scip-typescript` helper/CLI path.
/// When unset, the indexer falls back to `npx scip-typescript`.
pub const SCIP_TYPESCRIPT_HELPER_ENV: &str = "CODESEARCH_SCIP_TYPESCRIPT";

/// LMDB metadata key for the TypeScript indexer's last rebuild timestamp.
/// Namespaced per-language (unlike C#'s un-namespaced `SCIP_REBUILD_TIMESTAMP_KEY`)
/// so both adapters can safely share the same `scip_meta` table if ever merged.
pub const SCIP_TYPESCRIPT_REBUILD_TIMESTAMP_KEY: &str = "last_rebuild_ts:typescript";

/// TypeScript-specific key for `SCIP_HEAD_SHA_KEY` (the C# and TypeScript
/// adapters share one `scip_meta` table, so keys are language-prefixed).
pub const SCIP_TYPESCRIPT_HEAD_SHA_KEY: &str = "head_sha:typescript";

/// Debounce window (ms) for the TypeScript file-watcher symbol rebuild.
/// Mirrors `SCIP_CSHARP_DEBOUNCE_MS` — a single quiet-period flush avoids
/// spawning `scip-typescript` once per saved file during a burst of edits.
pub const SCIP_TYPESCRIPT_DEBOUNCE_MS: u64 = 60_000; // 60 seconds

/// Environment variable controlling phase-2 C# SCIP rebuild concurrency.
/// Parsed in `ServeState::csharp_scip_concurrency()` and clamped to [1, 4].
pub const CSHARP_SCIP_CONCURRENCY_ENV: &str = "CSHARP_SCIP_CONCURRENCY";

/// Default value for `CSHARP_SCIP_CONCURRENCY` when the env var is unset
/// or unparseable. Clamped to `[1, 4]` at the call site, so this default
/// is also expected to live within that range.
pub const CSHARP_SCIP_CONCURRENCY_DEFAULT: usize = 2;

/// Environment variable controlling Phase 3 pre-warm of reference cache.
/// When "true" (default), `run_phase_3_prewarm()` batch-resolves all uncached
/// symbol references after Phase 2 completes. Set to "false" on memory-constrained
/// machines to skip the workspace-open cost.
pub const CSHARP_PREWARM_ENABLED_ENV: &str = "CSHARP_PREWARM_ENABLED";

/// Maximum number of symbols to resolve per repo in Phase 3 pre-warm.
/// Limits the batch size to avoid excessive memory usage on large solutions.
pub const CSHARP_PREWARM_MAX_SYMBOLS: usize = 5000;

/// Maximum number of changed files chunked + embedded in a single in-memory
/// batch during `IndexManager::perform_incremental_refresh_with_stores`.
///
/// Without this cap, a single incremental refresh pass would read, chunk, and
/// embed the ENTIRE delta (every changed/new file since the last refresh) in
/// one unbounded `Vec`, before writing anything to the stores. This is safe
/// for normal incremental deltas (tens of files) but OOM'd a 1 vCPU / 2 GiB
/// `codesearch-serve` container when a vendor `docs` corpus roughly doubled in
/// one sync (2509 -> 5666 files): the in-process warmup tried to chunk+embed
/// thousands of files at once, exceeded available memory, and crash-looped.
///
/// Batching bounds peak memory to O(batch), not O(total delta), so a corpus
/// delta of any size can no longer OOM the process — it just takes longer,
/// spread across sequential batches.
///
/// Override at runtime with `CODESEARCH_INCREMENTAL_BATCH_SIZE`.
pub const INCREMENTAL_REFRESH_BATCH_SIZE: usize = 200;

/// Environment variable to override `INCREMENTAL_REFRESH_BATCH_SIZE`.
pub const INCREMENTAL_REFRESH_BATCH_SIZE_ENV: &str = "CODESEARCH_INCREMENTAL_BATCH_SIZE";

/// Default LMDB map size (MB) for the SCIP symbol index per repo.
///
/// This is virtual address space, not physical memory. On POSIX and Windows the
/// OS only faults in pages that are actually written, so 512 MB has no RAM cost
/// on typical repos. Enterprise C# repos with thousands of symbols and their
/// reference caches (Phase 3) can push past the old 64 MB limit.
///
/// Override at runtime with `CODESEARCH_SCIP_LMDB_MAP_MB` (integer, in MB).
pub const SCIP_LMDB_DEFAULT_MAP_SIZE_MB: usize = 512;

/// Environment variable to override the SCIP LMDB map size in megabytes.
/// When set, takes precedence over `SCIP_LMDB_DEFAULT_MAP_SIZE_MB`.
pub const SCIP_LMDB_MAP_SIZE_MB_ENV: &str = "CODESEARCH_SCIP_LMDB_MAP_MB";

/// Internal wall-clock budget (seconds) for a single `find_impact` reference
/// lookup.
///
/// A cold reference-cache miss makes the `find_impact` handler invoke the
/// external SCIP helper (`scip-csharp find-refs`), which can take several
/// minutes on a large solution. Without an internal deadline the MCP client
/// is the timeout mechanism: it aborts with an opaque `-32001 Request timed
/// out` and the calling agent falls back to plain-text search exactly when
/// the precise SCIP call graph is most useful. This budget makes the server
/// answer first with a structured busy envelope
/// (`{"busy": true, "state": ..., "waited_ms": ..., "advice": ...}`) while
/// the lookup continues in the background, so a retry is served warm from
/// the reference cache instead of cold again.
///
/// Override at runtime with `CODESEARCH_FIND_IMPACT_BUDGET_SECS` (integer
/// seconds). `0` disables the budget entirely, restoring the previous
/// unbounded-blocking behaviour. Unparseable values fall back to the default.
///
/// The default is deliberately BELOW typical MCP client timeouts (observed
/// live: an MCP client gave up at ~60s with `-32001` while the busy answer
/// was still being prepared at the 60s budget) — the structured busy answer
/// is only useful if it arrives before the client stops listening.
pub const DEFAULT_FIND_IMPACT_BUDGET_SECS: u64 = 45;

/// Environment variable to override `DEFAULT_FIND_IMPACT_BUDGET_SECS`.
pub const FIND_IMPACT_BUDGET_SECS_ENV: &str = "CODESEARCH_FIND_IMPACT_BUDGET_SECS";

/// How long (seconds) a budget-overrun `find_impact` lookup stays tracked
/// for retry observation. Must comfortably exceed the slowest legitimate
/// `scip-csharp find-refs` run (several minutes on a large solution): an
/// entry dropped while its lookup is still running would turn a retry into
/// a cold restart, voiding the dedupe the busy advice promises. Finished
/// entries are removed on their first retry read, so this cap only bounds
/// abandoned lookups.
pub const FIND_IMPACT_TRACK_TTL_SECS: u64 = 1800;

/// Debounce window (seconds) for persisting repos.json metadata updates.
/// Coalesces bursts of file changes into a single write.
pub const PERSIST_DEBOUNCE_SECS: u64 = 10;

/// File extensions that should never be indexed, regardless of content.
/// These are generated/compiled/binary-adjacent files with no semantic code value.
pub const ALWAYS_SKIP_EXTENSIONS: &[&str] = &[
    // Temporary / scratch files
    "tmp", "temp", "bak", "swp", "swo",  // Source maps (large, machine-generated)
    "map",  // Lock files
    "lock", // Package manifest locks
    "sum",  // go.sum
    // Compiled / bytecode output
    "pyc", "pyo", "pyd", "class", "o", "obj", "a", "lib", "so", "dll", "exe", "pdb", "ilk",
    // Archives
    "zip", "tar", "gz", "bz2", "xz", "7z", "rar", // Images / media
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "svg", "webp", "tiff", "mp3", "mp4", "wav", "ogg",
    "avi", "mov", "mkv", // Fonts
    "woff", "woff2", "ttf", "otf", "eot", // Database / binary data
    "db", "sqlite", "sqlite3", "mdb", "ldb", // Document formats (not source code)
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", // Certificates / keys
    "pem", "crt", "cer", "key", "p12", "pfx", // Generated protobuf / IDL
    "pb",
];

/// Filename suffix patterns that should never be indexed.
/// Matched against the full filename (case-insensitive).
/// Handles compound extensions like `.min.js` that the extension check cannot catch.
pub const ALWAYS_SKIP_FILENAME_SUFFIXES: &[&str] = &[
    // Minified web assets
    ".min.js",
    ".min.css",
    ".min.mjs",
    // Bundled / compiled JS
    ".bundle.js",
    ".chunk.js",
    ".esm.js",
    // TypeScript declaration files (generated, not source)
    ".d.ts",
    ".d.mts",
    ".d.cts",
    // Generated protobuf
    ".pb.go",
    ".pb.cc",
    ".pb.h",
    "_pb2.py",
    // Generated gRPC
    "_grpc.pb.go",
    "_grpc_pb.js",
    // Generated GraphQL
    ".generated.ts",
    ".generated.graphql",
    // Snapshot test output
    ".snap",
    // Editor swap / backup
    ".orig",
];

/// Directories and files that should always be excluded from indexing
/// These are added to both .gitignore and .codesearchignore automatically
pub const ALWAYS_EXCLUDED: &[&str] = &[
    // Codesearch databases
    ".codesearch",
    ".codesearch.db",
    ".codesearch.dbs",
    // Fastembed cache
    "fastembed_cache",
    // Version control
    ".git",
    ".svn",
    ".hg",
    // Build artifacts
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    // Python
    "__pycache__",
    ".pytest_cache",
    ".tox",
    "venv",
    ".venv",
    // Ruby
    "vendor",
    ".bundle",
    // Java
    ".gradle",
    ".m2",
    // IDE
    ".idea",
    ".vscode",
    ".vs",
    // Other
    "coverage",
    ".nyc_output",
    ".cache",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensure DEFAULT_SERVE_URL embeds the same port as DEFAULT_SERVE_PORT.
    /// If you bump DEFAULT_SERVE_PORT, you must also update DEFAULT_SERVE_URL.
    #[test]
    fn default_serve_url_matches_default_serve_port() {
        let port_str = DEFAULT_SERVE_PORT.to_string();
        assert!(
            DEFAULT_SERVE_URL.contains(&port_str),
            "DEFAULT_SERVE_URL ({DEFAULT_SERVE_URL}) does not contain DEFAULT_SERVE_PORT ({DEFAULT_SERVE_PORT}). \
             Update DEFAULT_SERVE_URL to match.",
        );
    }

    #[test]
    fn global_codesearchignore_path_returns_home_codesearch_dir() {
        let path = global_codesearchignore_path();
        assert!(path.is_some(), "Should return Some when home dir exists");
        let path = path.unwrap();
        assert!(
            path.to_string_lossy().contains(".codesearch"),
            "Path should contain .codesearch directory: {:?}",
            path
        );
        assert_eq!(
            path.file_name().unwrap(),
            GLOBAL_CODESEARCHIGNORE_FILE,
            "Filename should match GLOBAL_CODESEARCHIGNORE_FILE constant"
        );
    }
}
