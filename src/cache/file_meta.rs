use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::constants::FILE_META_DB_NAME;

// ─── CANONICAL PATH POLICY ────────────────────────────────────────────────────
//
// On Windows, `Path::canonicalize()` returns an extended-length UNC path of the
// form `\\?\C:\...`. Passing this prefix to `.join()`, `.exists()`, or storing
// it in repos.json causes inconsistent behaviour: `\\?\C:\foo\.codesearch.db`
// may return `false` from `Path::exists()` even when `C:\foo\.codesearch.db`
// exists, and HashMap keys built from UNC paths diverge from keys built from
// plain paths on the same directory.
//
// RULE: **Never call `.canonicalize()` directly.** Always use `safe_canonicalize()`
// instead. It is the single, central entry point that strips the prefix and
// returns a plain, reliable path suitable for storage and all filesystem ops.
//
// ─────────────────────────────────────────────────────────────────────────────

/// Strip the Windows extended-length UNC prefix (`\\?\`) from a canonicalized
/// path, returning a plain `C:\...` path. Idempotent on all other inputs.
///
/// This is exposed publicly so callers that already have a `PathBuf` and want
/// to strip the prefix without re-canonicalizing can do so.
pub fn strip_unc_prefix(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        PathBuf::from(stripped.to_string())
    } else {
        path
    }
}

/// Canonicalize a path and strip any Windows UNC `\\?\` prefix.
///
/// **This is the ONLY approved way to canonicalize paths in codesearch.**
/// It returns the same error as `Path::canonicalize()` on failure (path does
/// not exist, permission denied, etc.) and a clean `C:\...` path on success.
///
/// # Why not `.canonicalize()` directly?
/// On Windows `canonicalize()` returns `\\?\C:\...`. That prefix causes
/// `.join()` and `Path::exists()` to fail inconsistently on sub-paths, and
/// produces diverging HashMap keys when the same directory is accessed with
/// and without the prefix. `safe_canonicalize` eliminates this class of bug.
pub fn safe_canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    path.canonicalize().map(strip_unc_prefix)
}

/// Normalize a file path for consistent HashMap lookups.
///
/// On Windows, `Path::canonicalize()` and some APIs add a UNC extended-length
/// prefix (`\\?\C:\...`). Notify (FSW) events may use standard paths (`C:\...`).
/// This function strips the UNC prefix and converts backslashes to forward slashes
/// so that paths from different sources all map to the same key.
///
/// **Platform behavior** (Aikido group 30641757, priority 46):
/// - **Windows**: backslash IS a path separator — converting it to `/` is
///   required for HashMap consistency across APIs.
/// - **Unix**: backslash is a **legal filename character** (not a separator).
///   A file literally named `foo\bar.rs` is distinct from `foo/bar.rs` (which
///   lives in subdirectory `foo`). Unconditionally converting `\` → `/` would
///   collapse these two unrelated files into one HashMap key, causing silent
///   metadata corruption (one file's chunks overwrite the other's).
pub fn normalize_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    normalize_path_str(&s)
}

/// Normalize a path string (same logic as `normalize_path` but for `&str` input).
///
/// See `normalize_path` for the platform-specific separator handling and
/// the Aikido 30641757 rationale.
pub fn normalize_path_str(path: &str) -> String {
    let trimmed = path.trim_start_matches(r"\\?\");
    #[cfg(windows)]
    {
        trimmed.replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        // Backslash is a legal filename char on Unix — preserve it literally.
        // UNC prefix is already stripped above (it's a no-op on Unix in
        // practice, but defensive in case a Windows-style path string leaks in).
        trimmed.to_string()
    }
}

/// Normalize a filter path for prefix matching.
///
/// - Converts backslashes to forward slashes
/// - Removes leading `./`
/// - Removes trailing `/`
pub fn normalize_filter_path(filter: &str) -> String {
    normalize_path_str(filter)
        .trim_start_matches("./")
        .to_string()
}

/// Normalize a path and convert it to a project-relative path when possible.
///
/// `project_root_normalized` should be pre-normalized with `normalize_path_str`
/// (to avoid re-normalizing the same root in hot loops).
pub fn normalize_path_relative(path: &str, project_root_normalized: &str) -> String {
    let normalized_path = normalize_path_str(path);
    let project_root = project_root_normalized.trim_end_matches('/');

    let (relative, stripped_project_root) = if project_root.is_empty() {
        (normalized_path.as_str(), false)
    } else if let Some(stripped) = normalized_path.strip_prefix(project_root) {
        (stripped, true)
    } else {
        (normalized_path.as_str(), false)
    };

    if stripped_project_root {
        relative
            .trim_start_matches('/')
            .trim_start_matches("./")
            .to_string()
    } else {
        relative.trim_start_matches("./").to_string()
    }
}

/// Check whether a path matches a normalized filter prefix.
///
/// `project_root_normalized` should be pre-normalized with `normalize_path_str`.
pub fn path_matches_filter(
    path: &str,
    filter_normalized: &str,
    project_root_normalized: &str,
) -> bool {
    let path_relative = normalize_path_relative(path, project_root_normalized);
    let filter = filter_normalized.trim_end_matches('/');

    if filter.is_empty() {
        return true;
    }

    if path_relative == filter {
        return true;
    }

    let mut prefix = String::with_capacity(filter.len() + 1);
    prefix.push_str(filter);
    prefix.push('/');
    path_relative.starts_with(&prefix)
}

/// Metadata for a single indexed file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMeta {
    /// SHA256 hash of file content
    pub hash: String,
    /// File modification time (for quick change detection)
    pub mtime: u64,
    /// File size in bytes
    pub size: u64,
    /// Number of chunks extracted from this file
    pub chunk_count: usize,
    /// Chunk IDs in the vector store (for deletion on update)
    pub chunk_ids: Vec<u32>,
}

/// Persistent store for file metadata - enables incremental indexing
///
/// Improvements over osgrep:
/// 1. Two-level check: mtime first (fast), hash only if mtime changed
/// 2. Tracks chunk IDs for efficient deletion on file update
/// 3. Stores chunk count for statistics
#[derive(Debug, Serialize, Deserialize)]
pub struct FileMetaStore {
    /// Map of absolute file path -> metadata
    files: HashMap<String, FileMeta>,
    /// Model used for indexing (invalidate if model changes)
    pub model_name: String,
    /// Dimensions of embeddings
    pub dimensions: usize,
    /// Last full index timestamp
    pub last_full_index: Option<u64>,
    /// Version for format compatibility
    version: u32,
}

impl FileMetaStore {
    const CURRENT_VERSION: u32 = 1;
    const FILENAME: &'static str = FILE_META_DB_NAME;

    /// Create a new empty store
    pub fn new(model_name: String, dimensions: usize) -> Self {
        Self {
            files: HashMap::new(),
            model_name,
            dimensions,
            last_full_index: None,
            version: Self::CURRENT_VERSION,
        }
    }

    /// Load from database directory, or create new if doesn't exist
    pub fn load_or_create(db_path: &Path, model_name: &str, dimensions: usize) -> Result<Self> {
        let meta_path = db_path.join(Self::FILENAME);

        if meta_path.exists() {
            let content = fs::read_to_string(&meta_path)?;
            let mut store: FileMetaStore = serde_json::from_str(&content)
                .map_err(|e| anyhow!("Failed to parse file metadata: {}", e))?;

            // Check if model changed - if so, invalidate everything
            if store.model_name != model_name || store.dimensions != dimensions {
                println!(
                    "⚠️  Model changed ({} -> {}), full re-index required",
                    store.model_name, model_name
                );
                store = Self::new(model_name.to_string(), dimensions);
            }

            // Migrate stored paths to normalized format (strip UNC prefix, forward slashes).
            // Existing stores may have Windows backslash paths or \\?\ prefixed paths.
            store.migrate_paths();

            Ok(store)
        } else {
            Ok(Self::new(model_name.to_string(), dimensions))
        }
    }

    /// Save to database directory
    pub fn save(&self, db_path: &Path) -> Result<()> {
        let meta_path = db_path.join(Self::FILENAME);
        let content = serde_json::to_string_pretty(self)?;
        fs::write(meta_path, content)?;
        Ok(())
    }

    /// Migrate stored paths to normalized format.
    ///
    /// Existing stores may have Windows backslash paths (`C:\foo\bar.rs`) or
    /// UNC prefixed paths (`\\?\C:\foo\bar.rs`). This re-keys the HashMap
    /// to use the canonical normalized form (forward slashes, no UNC prefix).
    fn migrate_paths(&mut self) {
        let old_files = std::mem::take(&mut self.files);
        let capacity = old_files.len();
        let mut new_files = HashMap::with_capacity(capacity);
        let mut migrated = 0;

        for (old_key, meta) in old_files {
            let new_key = normalize_path_str(&old_key);
            if new_key != old_key {
                migrated += 1;
            }
            new_files.insert(new_key, meta);
        }

        self.files = new_files;

        if migrated > 0 {
            tracing::info!("🔄 Migrated {} file paths to normalized format", migrated);
        }
    }

    /// Compute SHA256 hash of file content
    pub fn compute_hash(path: &Path) -> Result<String> {
        let content = fs::read(path)?;
        let mut hasher = Sha256::new();
        hasher.update(&content);
        Ok(format!("{:x}", hasher.finalize()))
    }

    /// Get file modification time as unix timestamp
    fn get_mtime(path: &Path) -> Result<u64> {
        let metadata = fs::metadata(path)?;
        let mtime = metadata.modified()?;
        Ok(mtime.duration_since(SystemTime::UNIX_EPOCH)?.as_secs())
    }

    /// Check if a file needs re-indexing
    /// Check whether a path is already tracked (regardless of chunk count).
    /// Used by doctor to distinguish "never indexed" from "indexed but unchunkable".
    pub fn is_tracked(&self, path: &Path) -> bool {
        let path_str = normalize_path(path);
        self.files.contains_key(&path_str)
    }

    /// Returns: (needs_reindex, existing_chunk_ids_to_delete)
    pub fn check_file(&self, path: &Path) -> Result<(bool, Vec<u32>)> {
        let path_str = normalize_path(path);

        // Get current file stats
        let current_mtime = Self::get_mtime(path)?;
        let current_size = fs::metadata(path)?.len();

        if let Some(meta) = self.files.get(&path_str) {
            // Quick check: if mtime and size unchanged, file is unchanged
            if meta.mtime == current_mtime && meta.size == current_size {
                return Ok((false, vec![]));
            }

            // Mtime changed - compute hash to be sure
            let current_hash = Self::compute_hash(path)?;
            if meta.hash == current_hash {
                // Content same, just update mtime
                return Ok((false, vec![]));
            }

            // File changed - return old chunk IDs for deletion
            Ok((true, meta.chunk_ids.clone()))
        } else {
            // New file
            Ok((true, vec![]))
        }
    }

    /// Update metadata for a file after indexing
    pub fn update_file(&mut self, path: &Path, chunk_ids: Vec<u32>) -> Result<()> {
        let path_str = normalize_path(path);
        let hash = Self::compute_hash(path)?;
        let mtime = Self::get_mtime(path)?;
        let size = fs::metadata(path)?.len();

        self.files.insert(
            path_str,
            FileMeta {
                hash,
                mtime,
                size,
                chunk_count: chunk_ids.len(),
                chunk_ids,
            },
        );

        Ok(())
    }

    /// Mark a file as deleted
    pub fn remove_file(&mut self, path: &Path) -> Option<FileMeta> {
        let path_str = normalize_path(path);
        self.files.remove(&path_str)
    }

    /// Get all tracked files
    #[allow(dead_code)] // Reserved for file listing feature
    pub fn tracked_files(&self) -> impl Iterator<Item = &String> {
        self.files.keys()
    }

    /// Returns true if no files are tracked (metadata was reset or never created).
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Find files that were deleted (exist in store but not on disk)
    pub fn find_deleted_files(&self) -> Vec<(String, Vec<u32>)> {
        self.files
            .iter()
            .filter(|(path, _)| !Path::new(path).exists())
            .map(|(path, meta)| (path.clone(), meta.chunk_ids.clone()))
            .collect()
    }

    /// Get statistics
    #[allow(dead_code)] // Reserved for stats display
    pub fn stats(&self) -> FileMetaStats {
        let total_chunks: usize = self.files.values().map(|m| m.chunk_count).sum();
        let total_size: u64 = self.files.values().map(|m| m.size).sum();

        FileMetaStats {
            total_files: self.files.len(),
            total_chunks,
            total_size_bytes: total_size,
        }
    }

    /// Clear all entries (for full re-index)
    #[allow(dead_code)] // Reserved for index reset
    pub fn clear(&mut self) {
        self.files.clear();
        self.last_full_index = None;
    }

    /// Set last full index time
    #[allow(dead_code)]
    pub fn mark_full_index(&mut self) {
        self.last_full_index = Some(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        );
    }
}

#[derive(Debug)]
#[allow(dead_code)] // Used with stats() method
pub struct FileMetaStats {
    pub total_files: usize,
    pub total_chunks: usize,
    pub total_size_bytes: u64,
}

impl FileMetaStats {
    #[allow(dead_code)] // Reserved for stats display
    pub fn total_size_mb(&self) -> f64 {
        self.total_size_bytes as f64 / (1024.0 * 1024.0)
    }
}

#[cfg(test)]
#[path = "file_meta_tests.rs"]
mod tests;
