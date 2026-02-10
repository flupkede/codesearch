use anyhow::Result;
use ignore::WalkBuilder;
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::{debug, info, warn};

use crate::constants::ALWAYS_EXCLUDED;

mod binary;
mod language;

pub use binary::is_binary_file;
pub use language::Language;

/// Information about a discovered file
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub path: PathBuf,
    pub language: Language,
    pub size: u64,
}

/// Statistics about walked files
#[derive(Debug, Default, Clone)]
#[allow(dead_code)] // skipped_ignored reserved for future ignore stats
pub struct WalkStats {
    pub total_files: usize,
    pub indexable_files: usize,
    pub skipped_binary: usize,
    pub skipped_ignored: usize,
    pub files_by_language: HashMap<Language, usize>,
    pub total_size_bytes: u64,
}

impl WalkStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_file(&mut self, file: &FileInfo) {
        self.indexable_files += 1;
        self.total_size_bytes += file.size;
        *self.files_by_language.entry(file.language).or_insert(0) += 1;
    }

    pub fn add_skipped_binary(&mut self) {
        self.skipped_binary += 1;
    }

    pub fn total_size_mb(&self) -> f64 {
        self.total_size_bytes as f64 / (1024.0 * 1024.0)
    }

    pub fn print_summary(&self) {
        info!("File discovery complete:");
        info!("  Total files found: {}", self.total_files);
        info!("  Indexable files: {}", self.indexable_files);
        info!("  Binary/skipped: {}", self.skipped_binary);
        info!("  Total size: {:.2} MB", self.total_size_mb());

        if !self.files_by_language.is_empty() {
            info!("  Files by language:");
            let mut langs: Vec<_> = self.files_by_language.iter().collect();
            langs.sort_by(|a, b| b.1.cmp(a.1)); // Sort by count descending
            for (lang, count) in langs.iter().take(10) {
                info!("    {}: {}", lang.name(), count);
            }
        }
    }
}

/// Smart file walker that respects .gitignore and .codesearchignore
pub struct FileWalker {
    root: PathBuf,
    respect_gitignore: bool,
    include_hidden: bool,
}

impl FileWalker {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            respect_gitignore: true,
            include_hidden: false,
        }
    }

    /// Walk files, returning detailed file information
    pub fn walk(&self) -> Result<(Vec<FileInfo>, WalkStats)> {
        let mut files = Vec::new();
        let mut stats = WalkStats::new();

        debug!("Starting file walk in: {}", self.root.display());

        let mut builder = WalkBuilder::new(&self.root);
        builder
            .git_ignore(self.respect_gitignore)
            .git_global(self.respect_gitignore)
            .git_exclude(self.respect_gitignore)
            .hidden(!self.include_hidden)
            .add_custom_ignore_filename(".codesearchignore")
            .add_custom_ignore_filename(".osgrepignore") // Compatibility with osgrep
            // Filter out excluded directories BEFORE descending into them
            .filter_entry(|entry| {
                // Always allow the root entry
                if entry.depth() == 0 {
                    return true;
                }

                // Check if this entry's name is in the excluded list
                if let Some(name) = entry.file_name().to_str() {
                    if ALWAYS_EXCLUDED.contains(&name) {
                        debug!("Excluding directory: {}", entry.path().display());
                        return false;
                    }
                }
                true
            });

        for result in builder.build() {
            match result {
                Ok(entry) => {
                    stats.total_files += 1;

                    // Only process files (not directories)
                    let file_type = entry.file_type();
                    if file_type.is_none() || !file_type.unwrap().is_file() {
                        continue;
                    }

                    let path = entry.path();

                    // Check if file is binary
                    if is_binary_file(path) {
                        stats.add_skipped_binary();
                        debug!("Skipping binary file: {}", path.display());
                        continue;
                    }

                    // Get file info
                    let language = Language::from_path(path);

                    // Skip unknown/non-indexable files
                    if !language.is_indexable() {
                        stats.add_skipped_binary();
                        continue;
                    }

                    let size = entry.metadata().ok().map(|m| m.len()).unwrap_or(0);

                    let file_info = FileInfo {
                        path: path.to_path_buf(),
                        language,
                        size,
                    };

                    stats.add_file(&file_info);
                    files.push(file_info);
                }
                Err(err) => {
                    warn!("Error walking file: {}", err);
                }
            }
        }

        stats.print_summary();

        Ok((files, stats))
    }

    /// Walk files, returning just the paths (simpler API)
    #[allow(dead_code)] // Convenience method for simpler use cases
    pub fn walk_paths(&self) -> Result<Vec<PathBuf>> {
        let (files, _) = self.walk()?;
        Ok(files.into_iter().map(|f| f.path).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_file_walker_basic() {
        let dir = TempDir::new().unwrap();

        // Create some test files
        fs::write(dir.path().join("test.rs"), "fn main() {}").unwrap();
        fs::write(dir.path().join("test.py"), "print('hello')").unwrap();
        fs::write(dir.path().join("README.md"), "# Test").unwrap();

        let walker = FileWalker::new(dir.path());
        let (files, stats) = walker.walk().unwrap();

        assert_eq!(files.len(), 3);
        assert_eq!(stats.indexable_files, 3);
    }

    #[test]
    fn test_skip_binary_files() {
        let dir = TempDir::new().unwrap();

        // Create text file
        fs::write(dir.path().join("test.txt"), "hello world").unwrap();

        // Create binary file
        let bin_path = dir.path().join("test.bin");
        fs::write(&bin_path, [0u8, 1, 2, 3, 255]).unwrap();

        let walker = FileWalker::new(dir.path());
        let (files, stats) = walker.walk().unwrap();

        // Should only get the text file
        assert_eq!(files.len(), 1);
        assert!(stats.skipped_binary > 0);
    }

    #[test]
    fn test_language_detection() {
        let dir = TempDir::new().unwrap();

        fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        fs::write(dir.path().join("script.py"), "pass").unwrap();
        fs::write(dir.path().join("app.js"), "console.log()").unwrap();

        let walker = FileWalker::new(dir.path());
        let (files, stats) = walker.walk().unwrap();

        assert_eq!(files.len(), 3);
        assert_eq!(stats.files_by_language.get(&Language::Rust), Some(&1));
        assert_eq!(stats.files_by_language.get(&Language::Python), Some(&1));
        assert_eq!(stats.files_by_language.get(&Language::JavaScript), Some(&1));
    }

    #[test]
    fn test_excluded_directories() {
        let dir = TempDir::new().unwrap();

        // Create file in excluded directory
        let node_modules = dir.path().join("node_modules");
        fs::create_dir(&node_modules).unwrap();
        fs::write(node_modules.join("package.js"), "test").unwrap();

        // Create normal file
        fs::write(dir.path().join("index.js"), "test").unwrap();

        let walker = FileWalker::new(dir.path());
        let (files, _) = walker.walk().unwrap();

        // Should only get index.js, not the node_modules file
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path.file_name().unwrap(), "index.js");
    }
}
