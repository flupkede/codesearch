use anyhow::{anyhow, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, FileIdMap};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::cache::normalize_path;
use crate::constants::{ALWAYS_EXCLUDED, ALWAYS_SKIP_EXTENSIONS, ALWAYS_SKIP_FILENAME_SUFFIXES};
use crate::file::Language;

/// Normalize a path from notify events to a consistent format.
/// Strips UNC prefix (`\\?\`) and converts backslashes to forward slashes
/// so paths match the format used by FileMetaStore and VectorStore.
fn normalize_event_path(path: &Path) -> PathBuf {
    PathBuf::from(normalize_path(path))
}

/// Change information from git HEAD file.
///
/// Contains both the old and new HEAD content when a branch switch is detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadChange {
    /// Previous HEAD content (e.g., "ref: refs/heads/main\n")
    pub old_head: String,
    /// New HEAD content (e.g., "ref: refs/heads/feature\n")
    pub new_head: String,
}

/// Types of file system events we care about
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // Renamed variant reserved for future rename detection
pub enum FileEvent {
    /// File was created or modified
    Modified(PathBuf),
    /// File was deleted
    Deleted(PathBuf),
    /// File was renamed (from, to)
    Renamed(PathBuf, PathBuf),
}

/// File watcher for incremental indexing
///
/// Uses notify-debouncer-full for efficient debounced file watching.
/// Improvements over osgrep:
/// 1. Native Rust implementation (faster than Node.js chokidar)
/// 2. Built-in debouncing (configurable)
/// 3. Batched events for efficient processing
pub struct FileWatcher {
    root: PathBuf,
    debouncer: Option<Debouncer<RecommendedWatcher, FileIdMap>>,
    receiver: Option<Receiver<DebounceEventResult>>,
}

impl FileWatcher {
    /// Create a new file watcher for the given root directory
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            debouncer: None,
            receiver: None,
        }
    }

    /// Start watching for file changes
    pub fn start(&mut self, debounce_ms: u64) -> Result<()> {
        let (tx, rx) = channel();

        let debouncer = new_debouncer(
            Duration::from_millis(debounce_ms),
            None, // No tick rate
            tx,
        )
        .map_err(|e| anyhow!("Failed to create file watcher: {}", e))?;

        self.receiver = Some(rx);
        self.debouncer = Some(debouncer);

        // Start watching the root directory
        if let Some(ref mut debouncer) = self.debouncer {
            debouncer
                .watcher()
                .watch(&self.root, RecursiveMode::Recursive)
                .map_err(|e| anyhow!("Failed to watch directory: {}", e))?;

            // Also watch with the cache (for file ID tracking)
            debouncer
                .cache()
                .add_root(&self.root, RecursiveMode::Recursive);
        }

        Ok(())
    }

    /// Check if the watcher is currently started (collecting events)
    pub fn is_started(&self) -> bool {
        self.debouncer.is_some()
    }

    /// Stop watching
    pub fn stop(&mut self) {
        if let Some(ref mut debouncer) = self.debouncer {
            let _ = debouncer.watcher().unwatch(&self.root);
        }
        self.debouncer = None;
        self.receiver = None;
    }

    /// Check if a path is in an ignored directory (.git, node_modules, etc.)
    /// Uses the shared ALWAYS_EXCLUDED constant so FSW and FileWalker agree.
    fn is_in_ignored_dir(&self, path: &Path) -> bool {
        for component in path.components() {
            if let Some(name) = component.as_os_str().to_str() {
                if ALWAYS_EXCLUDED.contains(&name) {
                    return true;
                }
            }
        }
        false
    }

    /// Check if a path should be watched.
    /// Uses the same logic as FileWalker so FSW and index agree on what is indexable:
    /// - Not in an ignored directory (ALWAYS_EXCLUDED)
    /// - Not a skip extension (ALWAYS_SKIP_EXTENSIONS)
    /// - Not a skip filename suffix (ALWAYS_SKIP_FILENAME_SUFFIXES)
    /// - Not 0 bytes
    /// - Language is indexable (Language::from_path)
    fn is_watchable(&self, path: &Path) -> bool {
        if self.is_in_ignored_dir(path) {
            return false;
        }

        // Skip hardcoded extensions (e.g. .tmp, .map, .lock)
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let ext_lower = ext.to_lowercase();
            if ALWAYS_SKIP_EXTENSIONS.contains(&ext_lower.as_str()) {
                return false;
            }
        }

        // Skip hardcoded filename suffixes (e.g. .min.js, .d.ts, .designer.cs)
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let lower = name.to_lowercase();
            if ALWAYS_SKIP_FILENAME_SUFFIXES
                .iter()
                .any(|&s| lower.ends_with(s))
            {
                return false;
            }
        }

        // Skip 0-byte files (empty build artifacts)
        if path.metadata().map(|m| m.len() == 0).unwrap_or(false) {
            return false;
        }

        // Language must be indexable
        Language::from_path(path).is_indexable()
    }

    /// Poll for file events (non-blocking)
    /// Returns a batch of deduplicated events
    pub fn poll_events(&self) -> Vec<FileEvent> {
        let Some(ref receiver) = self.receiver else {
            return vec![];
        };

        let mut events = Vec::new();
        let mut seen_paths = HashSet::new();

        // Drain all available events
        while let Ok(result) = receiver.try_recv() {
            match result {
                Ok(debounced_events) => {
                    for event in debounced_events {
                        for raw_path in &event.paths {
                            // Normalize path: strip UNC prefix, convert backslashes
                            let path = normalize_event_path(raw_path);

                            // Skip ignored directories
                            if self.is_in_ignored_dir(&path) || seen_paths.contains(&path) {
                                continue;
                            }
                            seen_paths.insert(path.clone());

                            // Convert to our event type
                            use notify::EventKind;
                            match event.kind {
                                EventKind::Create(_) | EventKind::Modify(_) => {
                                    // For creates/modifies, only process indexable files
                                    if self.is_watchable(&path) && raw_path.exists() {
                                        events.push(FileEvent::Modified(path));
                                    }
                                }
                                EventKind::Remove(_) => {
                                    // For removals, don't filter by extension - directory
                                    // deletions on Windows may only report the directory
                                    // path (no file extension), not individual files
                                    events.push(FileEvent::Deleted(path));
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Err(errors) => {
                    for error in errors {
                        tracing::warn!("File watch error: {:?}", error);
                    }
                }
            }
        }

        events
    }

    /// Block and wait for events (with timeout)
    pub fn wait_for_events(&self, timeout: Duration) -> Vec<FileEvent> {
        let Some(ref receiver) = self.receiver else {
            return vec![];
        };

        let mut events = Vec::new();
        let mut seen_paths = HashSet::new();

        // Wait for first event
        match receiver.recv_timeout(timeout) {
            Ok(result) => {
                self.process_debounce_result(result, &mut events, &mut seen_paths);
            }
            Err(_) => return events, // Timeout or disconnected
        }

        // Drain any additional events that came in
        while let Ok(result) = receiver.try_recv() {
            self.process_debounce_result(result, &mut events, &mut seen_paths);
        }

        events
    }

    fn process_debounce_result(
        &self,
        result: DebounceEventResult,
        events: &mut Vec<FileEvent>,
        seen_paths: &mut HashSet<PathBuf>,
    ) {
        match result {
            Ok(debounced_events) => {
                for event in debounced_events {
                    for raw_path in &event.paths {
                        // Normalize path: strip UNC prefix, convert backslashes
                        let path = normalize_event_path(raw_path);

                        // Skip ignored directories and duplicates
                        if self.is_in_ignored_dir(&path) || seen_paths.contains(&path) {
                            continue;
                        }
                        seen_paths.insert(path.clone());

                        use notify::EventKind;
                        match event.kind {
                            EventKind::Create(_) | EventKind::Modify(_) => {
                                // For creates/modifies, only process indexable files
                                if self.is_watchable(&path) && raw_path.exists() {
                                    events.push(FileEvent::Modified(path));
                                }
                            }
                            EventKind::Remove(_) => {
                                // For removals, don't filter by extension - directory
                                // deletions on Windows may only report the directory
                                // path (no file extension), not individual files
                                events.push(FileEvent::Deleted(path));
                            }
                            _ => {}
                        }
                    }
                }
            }
            Err(errors) => {
                for error in errors {
                    tracing::warn!("File watch error: {:?}", error);
                }
            }
        }
    }
}

impl Drop for FileWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Git HEAD watcher for detecting branch changes.
///
/// Resolves the `.git/HEAD` path once at construction (including worktree indirection),
/// then polls cheaply by reading a single file and comparing content.
#[derive(Clone)]
pub struct GitHeadWatcher {
    /// Resolved path to the HEAD file (e.g. /repo/.git/HEAD or worktree target)
    head_path: PathBuf,
    /// Cached last HEAD content for change detection (thread-safe)
    last_head_content: Arc<Mutex<Option<String>>>,
}

impl GitHeadWatcher {
    /// Create a new Git HEAD watcher.
    ///
    /// Resolves the actual HEAD file path at construction time, handling
    /// git worktrees where `.git` is a file containing `gitdir: ...`.
    ///
    /// # Arguments
    /// * `git_root` - Path to the git repository root directory
    pub fn new(git_root: PathBuf) -> Self {
        let head_path = Self::resolve_head_path(&git_root);
        tracing::debug!("👀 Git HEAD watcher: {}", head_path.display());
        Self {
            head_path,
            last_head_content: Arc::new(Mutex::new(None)),
        }
    }

    /// Resolve the actual HEAD file path, handling worktrees.
    fn resolve_head_path(git_root: &Path) -> PathBuf {
        let git_entry = git_root.join(".git");

        if git_entry.is_file() {
            // Git worktree: .git is a file containing "gitdir: ..."
            if let Ok(content) = std::fs::read_to_string(&git_entry) {
                if let Some(first_line) = content.lines().next() {
                    let gitdir_str = first_line
                        .strip_prefix("gitdir: ")
                        .unwrap_or(first_line)
                        .trim();
                    let resolved = PathBuf::from(gitdir_str);
                    let resolved = if resolved.is_relative() {
                        git_root.join(&resolved)
                    } else {
                        resolved
                    };
                    return resolved.join("HEAD");
                }
            }
        }

        // Normal git repository
        git_entry.join("HEAD")
    }

    /// Check if the HEAD file has changed since the last check.
    ///
    /// This is called every ~100ms from the event loop, so it must be cheap.
    /// Only reads a single small file and compares a string.
    ///
    /// Returns:
    /// - `Ok(Some(HeadChange))` when a branch switch is detected
    /// - `Ok(None)` when HEAD is unchanged or on first check
    /// - `Err` if the HEAD file cannot be read
    pub async fn check(&self) -> Result<Option<HeadChange>> {
        let current_content = std::fs::read_to_string(&self.head_path).map_err(|e| {
            anyhow!(
                "Failed to read HEAD file {}: {}",
                self.head_path.display(),
                e
            )
        })?;

        let mut last = self.last_head_content.lock().await;

        let result = match &*last {
            Some(prev) if prev != &current_content => Some(HeadChange {
                old_head: prev.clone(),
                new_head: current_content.clone(),
            }),
            None => {
                // First check — initialize, report no change
                *last = Some(current_content);
                return Ok(None);
            }
            _ => None,
        };

        if result.is_some() {
            tracing::info!("🔀 Git HEAD changed (branch switch or HEAD moved)");
            *last = Some(current_content);
        }

        Ok(result)
    }

    /// Get the current HEAD reference (branch name or commit hash).
    #[allow(dead_code)]
    pub fn get_current_head(&self) -> Result<String> {
        let content = std::fs::read_to_string(&self.head_path)
            .map_err(|e| anyhow!("Failed to read HEAD file: {}", e))?;
        Ok(content.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_is_watchable() {
        let watcher = FileWatcher::new(PathBuf::from("/tmp"));

        // Should NOT watch (ignored dirs)
        assert!(!watcher.is_watchable(Path::new("/tmp/.git/config")));
        assert!(!watcher.is_watchable(Path::new("/tmp/node_modules/foo/index.js")));
        assert!(!watcher.is_watchable(Path::new("/tmp/target/debug/main")));
        assert!(!watcher.is_watchable(Path::new("/tmp/.codesearch.db/data")));

        // Should NOT watch (non-indexable extensions)
        assert!(!watcher.is_watchable(Path::new("/tmp/Cargo.lock")));
        assert!(!watcher.is_watchable(Path::new("/tmp/debug.log")));
        assert!(!watcher.is_watchable(Path::new("/tmp/image.png")));
        assert!(!watcher.is_watchable(Path::new("/tmp/data.bin")));

        // SHOULD watch (code files)
        assert!(watcher.is_watchable(Path::new("/tmp/src/main.rs")));
        assert!(watcher.is_watchable(Path::new("/tmp/src/lib.ts")));
        assert!(watcher.is_watchable(Path::new("/tmp/Program.cs")));
        assert!(watcher.is_watchable(Path::new("/tmp/app.py")));

        // SHOULD watch (config files)
        assert!(watcher.is_watchable(Path::new("/tmp/config.json")));
        assert!(watcher.is_watchable(Path::new("/tmp/settings.yaml")));
        assert!(watcher.is_watchable(Path::new("/tmp/Cargo.toml")));
        assert!(watcher.is_watchable(Path::new("/tmp/appsettings.xml")));

        // SHOULD watch (special files)
        assert!(watcher.is_watchable(Path::new("/tmp/Dockerfile")));
        assert!(watcher.is_watchable(Path::new("/tmp/Makefile")));
    }

    #[test]
    #[ignore] // Requires actual filesystem events
    fn test_file_watcher() {
        let dir = tempdir().unwrap();
        let mut watcher = FileWatcher::new(dir.path().to_path_buf());

        watcher.start(100).unwrap();

        // Create a file
        let test_file = dir.path().join("test.rs");
        fs::write(&test_file, "fn main() {}").unwrap();

        // Wait for events
        std::thread::sleep(Duration::from_millis(200));
        let events = watcher.poll_events();

        assert!(!events.is_empty());
    }
}
