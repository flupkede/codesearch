use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::SystemTime;

use crate::constants::FILE_META_DB_NAME;

/// Normalize a file path for consistent HashMap lookups.
///
/// On Windows, `Path::canonicalize()` and some APIs add a UNC extended-length
/// prefix (`\\?\C:\...`). Notify (FSW) events may use standard paths (`C:\...`).
/// This function strips the UNC prefix and converts backslashes to forward slashes
/// so that paths from different sources all map to the same key.
pub fn normalize_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    s.trim_start_matches(r"\\?\").replace('\\', "/")
}

/// Normalize a path string (same logic as `normalize_path` but for `&str` input).
pub fn normalize_path_str(path: &str) -> String {
    path.trim_start_matches(r"\\?\").replace('\\', "/")
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
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_normalize_path_strips_unc_prefix() {
        let path = Path::new(r"\\?\C:\WorkArea\AI\codesearch\src\main.rs");
        assert_eq!(
            normalize_path(path),
            "C:/WorkArea/AI/codesearch/src/main.rs"
        );
    }

    #[test]
    fn test_normalize_path_converts_backslashes() {
        let path = Path::new(r"C:\WorkArea\AI\codesearch\src\main.rs");
        assert_eq!(
            normalize_path(path),
            "C:/WorkArea/AI/codesearch/src/main.rs"
        );
    }

    #[test]
    fn test_normalize_path_forward_slashes_unchanged() {
        let path = Path::new("C:/WorkArea/AI/codesearch/src/main.rs");
        let result = normalize_path(path);
        // On Windows, Path::new with forward slashes may or may not convert them
        // The important thing is the result is consistent
        assert!(!result.contains('\\'));
        assert!(!result.starts_with(r"\\?\"));
    }

    #[test]
    fn test_normalize_path_str_strips_unc() {
        assert_eq!(normalize_path_str(r"\\?\C:\foo\bar.rs"), "C:/foo/bar.rs");
    }

    #[test]
    fn test_migrate_paths_normalizes_keys() {
        let mut store = FileMetaStore::new("test-model".to_string(), 384);
        // Insert with non-normalized key (simulating old format)
        store.files.insert(
            r"C:\WorkArea\src\main.rs".to_string(),
            FileMeta {
                hash: "abc123".to_string(),
                mtime: 1000,
                size: 100,
                chunk_count: 2,
                chunk_ids: vec![1, 2],
            },
        );
        store.files.insert(
            r"\\?\C:\WorkArea\src\lib.rs".to_string(),
            FileMeta {
                hash: "def456".to_string(),
                mtime: 2000,
                size: 200,
                chunk_count: 3,
                chunk_ids: vec![3, 4, 5],
            },
        );

        store.migrate_paths();

        // Both should be normalized
        assert!(store.files.contains_key("C:/WorkArea/src/main.rs"));
        assert!(store.files.contains_key("C:/WorkArea/src/lib.rs"));
        // Old keys should be gone
        assert!(!store.files.contains_key(r"C:\WorkArea\src\main.rs"));
        assert!(!store.files.contains_key(r"\\?\C:\WorkArea\src\lib.rs"));
    }

    #[test]
    fn test_file_meta_store() {
        let dir = tempdir().unwrap();
        let db_path = dir.path();

        let mut store = FileMetaStore::new("test-model".to_string(), 384);

        // Create a test file
        let test_file = dir.path().join("test.txt");
        fs::write(&test_file, "hello world").unwrap();

        // Check new file
        let (needs_reindex, old_chunks) = store.check_file(&test_file).unwrap();
        assert!(needs_reindex);
        assert!(old_chunks.is_empty());

        // Update metadata
        store.update_file(&test_file, vec![1, 2, 3]).unwrap();

        // Check again - should not need reindex
        let (needs_reindex, _) = store.check_file(&test_file).unwrap();
        assert!(!needs_reindex);

        // Modify file
        fs::write(&test_file, "hello world modified").unwrap();

        // Now should need reindex
        let (needs_reindex, old_chunks) = store.check_file(&test_file).unwrap();
        assert!(needs_reindex);
        assert_eq!(old_chunks, vec![1, 2, 3]);

        // Save and load
        store.save(db_path).unwrap();
        let loaded = FileMetaStore::load_or_create(db_path, "test-model", 384).unwrap();
        assert_eq!(loaded.files.len(), 1);
    }
}
