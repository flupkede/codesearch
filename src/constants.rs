//! Central constants for codesearch configuration
//!
//! All string literals for paths, filenames, and configuration should be defined here
//! to avoid duplication and ensure consistency across the codebase.

use std::path::PathBuf;

/// Name of the database directory in project roots
pub const DB_DIR_NAME: &str = ".codesearch.db";

/// Name of the global config directory in user home
pub const CONFIG_DIR_NAME: &str = ".codesearch";

/// Name of the file metadata database
pub const FILE_META_DB_NAME: &str = "file_meta.json";

/// Subdirectory name for embedding models within the global config dir
const MODELS_SUBDIR: &str = "models";

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

/// Default LMDB map size in bytes (2GB).
///
/// This is the maximum virtual address space reserved for the memory-mapped database.
/// On Linux/macOS this is just an address space reservation (no physical RAM until data is written).
/// On Windows the file may be pre-allocated to this size.
/// Override with `CODESEARCH_LMDB_MAP_SIZE_MB` environment variable.
pub const DEFAULT_LMDB_MAP_SIZE_MB: usize = 2048;

/// Default embedding cache memory limit in MB.
///
/// The embedding cache stores recently computed embeddings in memory (Moka LRU cache)
/// to avoid re-computing them during incremental indexing. This is real physical memory.
/// Override with `CODESEARCH_CACHE_MAX_MEMORY` environment variable.
pub const DEFAULT_CACHE_MAX_MEMORY_MB: usize = 200;

/// File watcher debounce time in milliseconds
pub const DEFAULT_FSW_DEBOUNCE_MS: u64 = 2000;

/// Lock file name to indicate an active writer instance
/// This prevents multiple processes from writing to the same database
pub const WRITER_LOCK_FILE: &str = ".writer.lock";

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
