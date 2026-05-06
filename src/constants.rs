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

/// Name of the repos configuration file
pub const REPOS_CONFIG_FILE: &str = "repos.json";

/// Default LMDB map size in megabytes (1024MB).
///
/// This is the maximum virtual address space reserved for the memory-mapped database.
/// On Linux/macOS this is just an address space reservation (no physical RAM until data is written).
/// On Windows the file may be pre-allocated to this size, so keeping it small matters.
/// 1024MB is sufficient for most codebases (~200k chunks × ~5KB = ~1024MB).
/// Override with `CODESEARCH_LMDB_MAP_SIZE_MB` environment variable.
pub const DEFAULT_LMDB_MAP_SIZE_MB: usize = 1024;

/// Maximum LMDB map size in megabytes (8192MB = 8GB).
///
/// This is the hard upper limit for auto-resizing when MDB_MAP_FULL errors occur.
/// Prevents unbounded growth and potential disk exhaustion.
pub const MAX_LMDB_MAP_SIZE_MB: usize = 8192;

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

/// File watcher debounce time in milliseconds
pub const DEFAULT_FSW_DEBOUNCE_MS: u64 = 2000;

/// Lock file name to indicate an active writer instance
/// This prevents multiple processes from writing to the same database
pub const WRITER_LOCK_FILE: &str = ".writer.lock";

/// Default port for `codesearch serve` (MCP streamable HTTP mode).
/// Override with `--port` or `CODESEARCH_SERVE_PORT`.
pub const DEFAULT_SERVE_PORT: u16 = 39725;

/// Environment variable to override the serve port.
pub const SERVE_PORT_ENV: &str = "CODESEARCH_SERVE_PORT";

/// Health-check path served by `codesearch serve`.
pub const HEALTH_PATH: &str = "/health";

/// MCP endpoint path served by `codesearch serve` (streamable HTTP).
pub const MCP_ENDPOINT_PATH: &str = "/mcp";

/// Status endpoint path served by `codesearch serve`.
/// Returns JSON snapshot of all repo states, sessions, and CPU usage.
pub const STATUS_PATH: &str = "/status";

/// How long an open repo may remain idle (no queries) before it is evicted.
/// Eviction closes the DB handles, stops the FSW, and releases memory.
/// The repo is automatically re-opened on the next query.
/// Override with `CODESEARCH_REPO_IDLE_TIMEOUT_SECS`.
pub const REPO_IDLE_TIMEOUT_SECS: u64 = 30 * 60; // 30 minutes

/// How often the idle-reaper background task checks for repos to evict.
pub const REAPER_INTERVAL_SECS: u64 = 5 * 60; // 5 minutes

/// Environment variable to override the repo idle timeout.
pub const REPO_IDLE_TIMEOUT_ENV: &str = "CODESEARCH_REPO_IDLE_TIMEOUT_SECS";

/// Default embedding dimensions used when metadata is missing or unreadable.
pub const DEFAULT_EMBEDDING_DIMENSIONS: usize = 384;

/// Environment variable to override repos config file path.
pub const REPOS_CONFIG_ENV: &str = "CODESEARCH_REPOS_CONFIG";

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

/// LMDB metadata key for the last rebuild timestamp.
pub const SCIP_REBUILD_TIMESTAMP_KEY: &str = "last_rebuild_ts";

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

/// Environment variable controlling phase-2 C# SCIP rebuild concurrency.
/// Parsed in `ServeState::csharp_scip_concurrency()` and clamped to [1, 4].
pub const CSHARP_SCIP_CONCURRENCY_ENV: &str = "CSHARP_SCIP_CONCURRENCY";

/// Default value for `CSHARP_SCIP_CONCURRENCY` when the env var is unset
/// or unparseable. Clamped to `[1, 4]` at the call site, so this default
/// is also expected to live within that range.
pub const CSHARP_SCIP_CONCURRENCY_DEFAULT: usize = 2;

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
