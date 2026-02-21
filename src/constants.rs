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
/// Override with `CODESEARCH_EMBEDDING_CACHE_MAX_ENTRIES` environment variable.
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
