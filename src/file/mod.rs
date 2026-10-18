use anyhow::Result;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::WalkBuilder;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

use crate::constants::{
    global_codesearchignore_path, global_codesearchinclude_path, ALWAYS_EXCLUDED,
    ALWAYS_SKIP_EXTENSIONS, ALWAYS_SKIP_FILENAME_SUFFIXES,
};

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

    pub fn add_skipped_ignored(&mut self) {
        self.skipped_ignored += 1;
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

/// Build the whitelist matcher from `.codesearchinclude` (repo root) and the
/// global `~/.codesearch/.codesearchinclude` (lowest precedence). `None` when
/// neither exists — or when both exist but carry no rules (empty or
/// comments-only) — the caller then indexes everything the ignore rules
/// allow, exactly as before. When a matcher EXISTS, it is a whitelist: a
/// file is indexed only when the include rules match it (the file itself or
/// an ancestor — `src/` covers `src/a/b.rs`), and ignore rules still win
/// first: an include can never resurrect an ignored file.
///
/// Shared by the walker and the watcher so the two gates cannot drift.
pub(crate) fn build_include_matcher(root: &Path) -> Option<Gitignore> {
    let mut builder = GitignoreBuilder::new(root);
    let mut added_any = false;

    if let Some(global_path) = global_codesearchinclude_path() {
        if global_path.exists() {
            if let Some(e) = builder.add(&global_path) {
                debug!("Failed to add global codesearchinclude: {}", e);
            } else {
                added_any = true;
            }
        }
    }

    let local_path = root.join(".codesearchinclude");
    if local_path.exists() {
        if let Some(e) = builder.add(&local_path) {
            debug!("Failed to add .codesearchinclude: {}", e);
        } else {
            added_any = true;
        }
    }

    if !added_any {
        return None;
    }
    let matcher = builder.build().ok()?;
    // A present-but-empty file (no rules — e.g. only `#` comments) is not a
    // whitelist: `matched` would answer None for every path and index
    // nothing, contradicting the "absent or empty = historical behavior"
    // contract.
    if matcher.num_ignores() == 0 && matcher.num_whitelists() == 0 {
        return None;
    }
    Some(matcher)
}

/// Whitelist verdict for one path against the include matcher: `true` when
/// the file is included (the path itself or an ancestor matches an include
/// rule; a `!`-whitelist rule inside the include file un-includes again —
/// last match along the component chain wins, mirroring gitignore order).
pub(crate) fn is_included(matcher: &Gitignore, root: &Path, path: &Path) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let mut current = PathBuf::new();
    let mut included = false;
    for component in relative.components() {
        current.push(component);
        match matcher.matched(&current, true) {
            m if m.is_ignore() => included = true,
            m if m.is_whitelist() => included = false,
            _ => {}
        }
    }
    included
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
        // Security (Aikido group 30641794): refuse to walk a root whose own
        // name is on the always-excluded list (e.g. `.git`, `.svn`, `node_modules`).
        // The `filter_entry` closure below skips these names at depth >= 1, but
        // it short-circuits on `depth() == 0` (the root). Without this guard,
        // `codesearch index ./.git` would happily index every object, ref, and
        // config file under `.git/`, exposing internal/sensitive metadata via
        // search results. Fail fast at the walker's entry point so every caller
        // (CLI `index`, HTTP `/repos`, `doctor`, `sync_database`, watcher) is
        // covered uniformly.
        if let Some(name) = self.root.file_name().and_then(|n| n.to_str()) {
            if ALWAYS_EXCLUDED.contains(&name) {
                anyhow::bail!(
                    "Refusing to index '{}' — this directory name is on the \
                     always-excluded list (e.g. `.git`, `.svn`, `node_modules`). \
                     Indexing it would expose internal/sensitive files via search. \
                     Point the indexer at the parent project directory instead.",
                    self.root.display()
                );
            }
        }

        let mut files = Vec::new();
        let mut stats = WalkStats::new();

        debug!("Starting file walk in: {}", self.root.display());

        let mut builder = WalkBuilder::new(&self.root);
        // Whitelist mode: when a .codesearchinclude exists (repo or global),
        // ONLY files matching it are indexed. Absent = everything, as before.
        let include_matcher = build_include_matcher(&self.root);
        if include_matcher.is_some() {
            debug!("Include list active: indexing only .codesearchinclude matches");
        }

        builder
            .git_ignore(self.respect_gitignore)
            .git_global(self.respect_gitignore)
            .git_exclude(self.respect_gitignore)
            .hidden(!self.include_hidden)
            .add_custom_ignore_filename(".codesearchignore")
            .add_custom_ignore_filename(".osgrepignore"); // Compatibility with osgrep

        // Add global ~/.codesearch/.codesearchignore (applies to all repos)
        if let Some(global_ignore) = global_codesearchignore_path() {
            if global_ignore.exists() {
                debug!(
                    "Loading global codesearchignore: {}",
                    global_ignore.display()
                );
                builder.add_ignore(&global_ignore);
            }
        }

        builder
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

                    // Skip 0-byte files — nothing to index
                    let size = entry.metadata().ok().map(|m| m.len()).unwrap_or(0);
                    if size == 0 {
                        stats.add_skipped_binary();
                        debug!("Skipping empty file: {}", path.display());
                        continue;
                    }

                    // Skip always-excluded file extensions (e.g. .tmp, .map, .lock, .min.js)
                    if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
                        let fname_lower = fname.to_ascii_lowercase();
                        // Check compound suffix patterns first (.min.js, .d.ts, etc.)
                        if ALWAYS_SKIP_FILENAME_SUFFIXES
                            .iter()
                            .any(|s| fname_lower.ends_with(s))
                        {
                            stats.add_skipped_binary();
                            debug!("Skipping generated/minified file: {}", path.display());
                            continue;
                        }
                        // Check single extensions (.tmp, .map, .lock, etc.)
                        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                            if ALWAYS_SKIP_EXTENSIONS
                                .iter()
                                .any(|s| s.eq_ignore_ascii_case(ext))
                            {
                                stats.add_skipped_binary();
                                debug!("Skipping excluded extension .{}: {}", ext, path.display());
                                continue;
                            }
                        }
                    }

                    // Whitelist gate: with an active include list, a file
                    // that no include rule covers is not indexed. Runs AFTER
                    // the ignore machinery — an include can never resurrect
                    // an ignored file.
                    if let Some(inc) = &include_matcher {
                        if !is_included(inc, &self.root, path) {
                            stats.add_skipped_ignored();
                            debug!("Skipping file not in include list: {}", path.display());
                            continue;
                        }
                    }

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
    use crate::testing::EnvRestore;
    use serial_test::serial;
    use std::fs;
    use tempfile::TempDir;

    /// Pin every location the walker can find a global `.codesearchignore`
    /// (CODESEARCH_HOME and its HOME/USERPROFILE fallback) to fresh empty
    /// directories, so a developer's real global ignore list cannot prune the
    /// test fixtures — CI has no such file, which is how the ambient dependency
    /// went unnoticed (the pilot's global list starts with `*.bin`, which
    /// silently killed test_skip_binary_files on the dev machine).
    fn isolated_global_ignore_env() -> (TempDir, EnvRestore) {
        let scratch = TempDir::new().unwrap();
        let cs_home = scratch.path().join("cs-home");
        let bare_home = scratch.path().join("bare-home");
        fs::create_dir_all(&cs_home).unwrap();
        fs::create_dir_all(&bare_home).unwrap();
        let env = EnvRestore::set(&[
            (
                crate::constants::CODESEARCH_HOME_ENV,
                cs_home.to_str().unwrap(),
            ),
            ("HOME", bare_home.to_str().unwrap()),
            ("USERPROFILE", bare_home.to_str().unwrap()),
        ]);
        (scratch, env)
    }

    #[test]
    #[serial]
    fn test_file_walker_basic() {
        let _isolation = isolated_global_ignore_env();
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
    #[serial]
    fn test_skip_binary_files() {
        let _isolation = isolated_global_ignore_env();
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
    #[serial]
    fn test_language_detection() {
        let _isolation = isolated_global_ignore_env();
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
    #[serial]
    fn test_excluded_directories() {
        let _isolation = isolated_global_ignore_env();
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

    /// Dependency lock files are excluded by exact filename: their extensions
    /// (.yaml/.json) are indexable on their own, so without the name match the
    /// walker would index every churn commit of a generated lock file.
    #[test]
    #[serial]
    fn test_excluded_lock_files() {
        let _isolation = isolated_global_ignore_env();
        let dir = TempDir::new().unwrap();

        fs::write(dir.path().join("pnpm-lock.yaml"), "lockVersion: '9.0'").unwrap();
        fs::write(dir.path().join("package-lock.json"), "{}").unwrap();
        fs::write(dir.path().join("yarn.lock"), "# yarn lockfile").unwrap();
        fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();

        let walker = FileWalker::new(dir.path());
        let (files, _) = walker.walk().unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path.file_name().unwrap(), "main.rs");
    }

    /// A root whose own name matches an `ALWAYS_EXCLUDED` entry (e.g. `.git`)
    /// must be rejected at `walk()` time — otherwise the depth==0 short-circuit
    /// in `filter_entry` would let every internal file be indexed.
    /// Covers Aikido group 30641794.
    #[test]
    #[serial]
    fn test_rejects_excluded_named_root() {
        let _isolation = isolated_global_ignore_env();
        let parent = TempDir::new().unwrap();
        let git_root = parent.path().join(".git");
        fs::create_dir(&git_root).unwrap();
        fs::write(git_root.join("config"), "[core]").unwrap();
        fs::write(git_root.join("HEAD"), "ref: refs/heads/main").unwrap();

        let walker = FileWalker::new(&git_root);
        let err = walker.walk().unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("Refusing to index"),
            "expected refusal message, got: {}",
            msg
        );
        assert!(msg.contains(".git"), "message should name the offender");

        // Sanity: a non-excluded name in the same parent walks normally.
        let ok_root = parent.path().join("real_project");
        fs::create_dir(&ok_root).unwrap();
        fs::write(ok_root.join("main.rs"), "fn main() {}").unwrap();
        let (files, _) = FileWalker::new(&ok_root).walk().unwrap();
        assert_eq!(files.len(), 1);
    }

    #[test]
    #[serial]
    fn include_list_whitelists_matching_files_only() {
        let _isolation = isolated_global_ignore_env();
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("keep.rs"), "fn a() {}").unwrap();
        fs::write(dir.path().join("drop.py"), "print(1)").unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src").join("nested.rs"), "fn b() {}").unwrap();
        // Ancestor pattern `src/` must cover nested files.
        fs::write(dir.path().join(".codesearchinclude"), "*.rs\nsrc/\n").unwrap();

        let walker = FileWalker::new(dir.path());
        let (files, stats) = walker.walk().unwrap();

        let names: Vec<String> = files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"keep.rs".to_string()));
        assert!(names.contains(&"nested.rs".to_string()));
        assert!(!names.contains(&"drop.py".to_string()));
        assert_eq!(stats.indexable_files, 2);
        assert_eq!(stats.skipped_ignored, 1);
    }

    #[test]
    #[serial]
    fn include_list_negation_unincludes_again() {
        let _isolation = isolated_global_ignore_env();
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("keep.rs"), "fn a() {}").unwrap();
        fs::write(dir.path().join("gen.rs"), "generated").unwrap();
        fs::write(dir.path().join(".codesearchinclude"), "*.rs\n!gen.rs\n").unwrap();

        let walker = FileWalker::new(dir.path());
        let (files, _) = walker.walk().unwrap();

        let names: Vec<String> = files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"keep.rs".to_string()));
        assert!(!names.contains(&"gen.rs".to_string()));
    }

    #[test]
    #[serial]
    fn no_include_file_indexes_everything() {
        let _isolation = isolated_global_ignore_env();
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        fs::write(dir.path().join("b.py"), "print(1)").unwrap();

        let walker = FileWalker::new(dir.path());
        let (files, _) = walker.walk().unwrap();
        assert_eq!(files.len(), 2, "absent include list = historical behavior");
    }

    #[test]
    #[serial]
    fn include_list_ancestor_negation_unincludes_again() {
        let _isolation = isolated_global_ignore_env();
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src").join("keep.rs"), "fn a() {}").unwrap();
        fs::write(dir.path().join("src").join("gen.rs"), "generated").unwrap();
        fs::write(dir.path().join("drop.py"), "print(1)").unwrap();
        // The ancestor include decides first, the deeper `!` rule must win:
        // last decisive match along the component chain.
        fs::write(dir.path().join(".codesearchinclude"), "src/\n!src/gen.rs\n").unwrap();

        let walker = FileWalker::new(dir.path());
        let (files, stats) = walker.walk().unwrap();

        let names: Vec<String> = files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.contains(&"keep.rs".to_string()));
        assert!(
            !names.contains(&"gen.rs".to_string()),
            "the !-rule under an included ancestor must un-include"
        );
        assert!(!names.contains(&"drop.py".to_string()));
        assert_eq!(stats.indexable_files, 1);
        assert_eq!(stats.skipped_ignored, 2);
    }

    #[test]
    #[serial]
    fn include_list_present_but_empty_indexes_everything() {
        let _isolation = isolated_global_ignore_env();
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        fs::write(dir.path().join("b.py"), "print(1)").unwrap();
        // Comments only: no rules, so this is not a whitelist.
        fs::write(dir.path().join(".codesearchinclude"), "# nothing yet\n").unwrap();

        let walker = FileWalker::new(dir.path());
        let (files, _) = walker.walk().unwrap();
        assert_eq!(
            files.len(),
            2,
            "a present-but-empty include list = historical behavior"
        );
    }
}
