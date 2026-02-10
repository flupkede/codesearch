use anyhow::{anyhow, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, FileIdMap};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

use crate::cache::normalize_path;

/// Normalize a path from notify events to a consistent format.
/// Strips UNC prefix (`\\?\`) and converts backslashes to forward slashes
/// so paths match the format used by FileMetaStore and VectorStore.
fn normalize_event_path(path: &Path) -> PathBuf {
    PathBuf::from(normalize_path(path))
}

/// File extensions that should trigger re-indexing (whitelist approach)
/// This includes code files and configuration files
const INDEXABLE_EXTENSIONS: &[&str] = &[
    // Rust
    "rs",
    // JavaScript/TypeScript
    "js",
    "mjs",
    "cjs",
    "jsx",
    "ts",
    "mts",
    "cts",
    "tsx",
    // Python
    "py",
    "pyw",
    "pyi",
    // C/C++
    "c",
    "h",
    "cpp",
    "cc",
    "cxx",
    "hpp",
    "hxx",
    // C#
    "cs",
    "csx",
    // Java/Kotlin
    "java",
    "kt",
    "kts",
    // Go
    "go",
    // Ruby
    "rb",
    "rake",
    // PHP
    "php",
    // Swift
    "swift",
    // Shell/Scripts
    "sh",
    "bash",
    "zsh",
    "fish",
    "ps1",
    "psm1",
    "psd1",
    // Web
    "html",
    "htm",
    "css",
    "scss",
    "sass",
    "less",
    "vue",
    "svelte",
    // Config/Data
    "json",
    "jsonc",
    "json5",
    "yaml",
    "yml",
    "toml",
    "xml",
    "ini",
    "conf",
    "config",
    // .NET
    "csproj",
    "sln",
    "props",
    "targets",
    "razor",
    "cshtml",
    // SQL
    "sql",
    // Markdown/Docs
    "md",
    "markdown",
    "rst",
    // Other
    "graphql",
    "gql",
    "proto",
    "dockerfile",
];

/// Directories that should always be ignored
const IGNORED_DIRS: &[&str] = &[
    ".git",
    ".codesearch.db",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".cache",
    "dist",
    "build",
    "out",
    "bin",
    "obj",
    ".vs",
    ".idea",
    ".vscode",
    "packages",
    ".nuget",
];

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
    fn is_in_ignored_dir(&self, path: &Path) -> bool {
        for component in path.components() {
            if let Some(name) = component.as_os_str().to_str() {
                if IGNORED_DIRS.contains(&name) {
                    return true;
                }
            }
        }
        false
    }

    /// Check if a path should be watched (whitelist approach)
    /// Only returns true for indexable code/config files
    fn is_watchable(&self, path: &Path) -> bool {
        // Check if path is in an ignored directory
        if self.is_in_ignored_dir(path) {
            return false;
        }

        // Must be a file with an indexable extension
        if let Some(ext) = path.extension() {
            if let Some(ext_str) = ext.to_str() {
                return INDEXABLE_EXTENSIONS.contains(&ext_str.to_lowercase().as_str());
            }
        }

        // Special case: Dockerfile (no extension)
        if let Some(name) = path.file_name() {
            let name_str = name.to_string_lossy().to_lowercase();
            if name_str == "dockerfile" || name_str == "makefile" || name_str == "cmakelists.txt" {
                return true;
            }
        }

        false
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
