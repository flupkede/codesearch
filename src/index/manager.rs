//! Index management module with auto-refresh and file watching support.
//!
//! This module provides a unified interface for both MCP and HTTP server
//! to manage index lifecycle: initial load/refresh and background file watching.
//!
//! # Multi-instance Support
//!
//! When multiple processes need to access the same database (e.g., two terminal windows
//! in the same directory), this module supports:
//!
//! - **Writer mode**: First instance gets write access with file watching enabled
//! - **Readonly mode**: Subsequent instances open in readonly mode (no writes, no watcher)
//!
//! A lock file (`.writer.lock`) in the database directory indicates an active writer.
//!
#![allow(dead_code)]

use crate::cache::{normalize_path, normalize_path_str};
use crate::constants::{DB_DIR_NAME, DEFAULT_FSW_DEBOUNCE_MS, FILE_META_DB_NAME, WRITER_LOCK_FILE};
use crate::embed::ModelType;
use crate::fts::FtsStore;
use crate::vectordb::VectorStore;
use crate::watch::{FileEvent, FileWatcher, GitHeadWatcher};
use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

// Import Result from the parent module
use super::Result;

/// Batch flush timeout in milliseconds.
/// Events are batched and flushed when:
/// 1. No new events for this duration, OR
/// 2. Buffer has events and this duration passes since last flush
const FSW_BATCH_FLUSH_MS: u64 = 2000;

// === Lock File Management ===

/// Check if the database is currently locked by another process.
///
/// Returns `true` if another process has the write lock.
pub fn is_database_locked(db_path: &Path) -> bool {
    use fs2::FileExt;

    let lock_path = db_path.join(WRITER_LOCK_FILE);
    if !lock_path.exists() {
        return false;
    }

    // Try to acquire an exclusive lock on the file
    // If we can't, another process holds the lock
    match File::options().read(true).write(true).open(&lock_path) {
        Ok(file) => {
            // try_lock_exclusive returns Ok(()) if we got the lock, Err if not
            match file.try_lock_exclusive() {
                Ok(()) => {
                    // We got the lock, so it wasn't locked. Release it.
                    let _ = file.unlock();
                    false
                }
                Err(_) => {
                    // Could not acquire lock - another process has it
                    true
                }
            }
        }
        Err(_) => {
            // If we can't open the file, assume it's not locked
            // (file might not exist or permissions issue)
            false
        }
    }
}

/// Acquire the writer lock for the database.
///
/// Returns the lock file handle (keep it open to hold the lock).
/// Returns `None` if the lock is already held by another process.
pub fn acquire_writer_lock(db_path: &Path) -> Option<File> {
    use fs2::FileExt;

    let lock_path = db_path.join(WRITER_LOCK_FILE);

    // Create or open the lock file
    let file = match File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(e) => {
            warn!("Failed to open lock file: {}", e);
            return None;
        }
    };

    // Try to acquire exclusive lock (non-blocking)
    match file.try_lock_exclusive() {
        Ok(()) => {
            // Successfully acquired lock
            debug!("🔒 Writer lock acquired");
            Some(file)
        }
        Err(e) => {
            // Failed to acquire lock - another process holds it
            debug!("🔒 Failed to acquire writer lock: {}", e);
            None
        }
    }
}

/// Release the writer lock (done automatically when File is dropped)
#[allow(dead_code)]
pub fn release_writer_lock(_lock: File) {
    // Lock is released automatically when the File is dropped
    debug!("🔓 Writer lock released");
}

/// Shared stores for concurrent access between MCP service and file watcher.
///
/// Uses RwLock to allow multiple concurrent readers (searches) with exclusive writer (indexing).
pub struct SharedStores {
    pub vector_store: Arc<RwLock<VectorStore>>,
    pub fts_store: Arc<RwLock<FtsStore>>,
    /// Lock file handle (Some = we have writer lock, None = readonly mode)
    #[allow(dead_code)]
    writer_lock: Option<File>,
    /// Whether this instance is in readonly mode
    pub readonly: bool,
}

impl SharedStores {
    /// Create new shared stores from the database path (read-write mode).
    ///
    /// This acquires a writer lock. If another process already has the lock,
    /// this will fail with an error.
    pub fn new(db_path: &Path, dimensions: usize) -> Result<Self> {
        // Try to acquire writer lock
        let lock = acquire_writer_lock(db_path);
        if lock.is_none() {
            return Err(anyhow::anyhow!(
                "Database is locked by another process. Use new_readonly() instead."
            ));
        }

        let vector_store = VectorStore::new(db_path, dimensions)?;
        let fts_store = FtsStore::new_with_writer(db_path)?;

        info!("📦 SharedStores created in read-write mode");

        Ok(Self {
            vector_store: Arc::new(RwLock::new(vector_store)),
            fts_store: Arc::new(RwLock::new(fts_store)),
            writer_lock: lock,
            readonly: false,
        })
    }

    /// Create shared stores in readonly mode (for secondary instances).
    ///
    /// This does not acquire any locks and cannot write to the database.
    /// File watching is not supported in readonly mode.
    pub fn new_readonly(db_path: &Path, dimensions: usize) -> Result<Self> {
        let vector_store = VectorStore::open_readonly(db_path, dimensions)?;
        let fts_store = FtsStore::new(db_path)?; // Read-only without writer

        info!("📦 SharedStores created in readonly mode");

        Ok(Self {
            vector_store: Arc::new(RwLock::new(vector_store)),
            fts_store: Arc::new(RwLock::new(fts_store)),
            writer_lock: None,
            readonly: true,
        })
    }

    /// Try to create shared stores, falling back to readonly mode if locked.
    ///
    /// Returns (SharedStores, is_readonly) tuple.
    pub fn new_or_readonly(db_path: &Path, dimensions: usize) -> Result<(Self, bool)> {
        // First, check if locked
        if is_database_locked(db_path) {
            info!("🔒 Database is locked by another process, opening in readonly mode...");
            let stores = Self::new_readonly(db_path, dimensions)?;
            return Ok((stores, true));
        }

        // Try to create in write mode
        match Self::new(db_path, dimensions) {
            Ok(stores) => Ok((stores, false)),
            Err(e) => {
                // If failed to acquire lock, try readonly
                if e.to_string().contains("locked") {
                    info!("🔒 Failed to acquire lock, opening in readonly mode...");
                    let stores = Self::new_readonly(db_path, dimensions)?;
                    Ok((stores, true))
                } else {
                    Err(e)
                }
            }
        }
    }
}

/// Index manager that handles index lifecycle and file watching.
///
/// Provides two-phase initialization:
/// 1. `new()` - Load or refresh index at startup
/// 2. `start_file_watcher()` - Start background file watching
pub struct IndexManager {
    /// Path to the codebase to index
    codebase_path: PathBuf,
    /// Path to the database
    db_path: PathBuf,
    /// File watcher instance
    watcher: Arc<Mutex<FileWatcher>>,
    /// Git HEAD watcher for branch change detection
    git_head_watcher: Option<GitHeadWatcher>,
    /// Shared stores for concurrent access
    stores: Arc<SharedStores>,
}

impl IndexManager {
    /// Create a new index manager with shared stores.
    ///
    /// This is the **first method call** - should be called at server startup.
    ///
    /// # Arguments
    /// * `codebase_path` - Path to the codebase to index
    /// * `stores` - Shared stores for concurrent access (created by caller)
    ///
    /// # Returns
    /// * `Result<Self>` - Index manager instance or error
    ///
    /// # Behavior
    /// - Checks if index exists and is up-to-date
    /// - **ERROR if index doesn't exist** - user must run `codesearch index add` first
    /// - If index exists, performs incremental refresh
    /// - Logs all operations with detailed info
    ///
    /// # Errors
    /// - Returns error if index doesn't exist (user must create index first)
    pub async fn new<P: AsRef<Path>>(codebase_path: P, stores: Arc<SharedStores>) -> Result<Self> {
        let path_buf = codebase_path.as_ref().to_path_buf();
        let db_path = path_buf.join(DB_DIR_NAME);

        info!("🔍 Initializing index manager for: {}", path_buf.display());

        // Check if index exists
        let needs_initial = Self::needs_initial_indexing(&path_buf).await?;

        if needs_initial {
            // Index doesn't exist - ERROR, don't auto-create
            let error_msg = format!(
                "❌ No index found for: {}\n\n\
                 💡 To create an index, run one of these commands:\n\
                 • For local index:  codesearch index add\n\
                 • For global index: codesearch index add -g\n\n\
                 Then start the server again.",
                path_buf.display()
            );
            return Err(anyhow::anyhow!(error_msg));
        }

        // Index exists, perform incremental refresh
        info!("🔄 Index exists, performing incremental refresh...");
        Self::perform_incremental_refresh(&path_buf).await?;

        // Create file watcher (but don't start it yet)
        debug!("👀 Creating file watcher...");
        let watcher = FileWatcher::new(path_buf.clone());
        let watcher = Arc::new(Mutex::new(watcher));

        // Create Git HEAD watcher for branch change detection
        debug!("🔀 Creating Git HEAD watcher...");
        let git_head_watcher = Self::find_and_create_git_head_watcher(&path_buf)?;

        info!("✅ Index manager initialized successfully");

        Ok(Self {
            codebase_path: path_buf,
            db_path,
            watcher,
            git_head_watcher: Some(git_head_watcher),
            stores,
        })
    }

    /// Get a reference to the shared stores (for CodesearchService)
    pub fn stores(&self) -> Arc<SharedStores> {
        self.stores.clone()
    }

    /// Find and create Git HEAD watcher for branch change detection.
    ///
    /// This method attempts to find the git repository root and creates
    /// a GitHeadWatcher to monitor for branch changes. If not in a git
    /// repository, returns a disabled watcher.
    ///
    /// # Arguments
    ///
    /// * `codebase_path` - Path to the codebase
    ///
    /// # Returns
    ///
    /// * `Result<GitHeadWatcher>` - Git HEAD watcher or error
    fn find_and_create_git_head_watcher(codebase_path: &Path) -> Result<GitHeadWatcher> {
        // Try to find git root using the index module's find_git_root function
        let git_root = match crate::index::find_git_root(codebase_path) {
            Ok(Some(root)) => root,
            Ok(None) => {
                // Not in a git repository, return a disabled watcher
                debug!("Not in a git repository, Git HEAD watcher disabled");
                return Ok(GitHeadWatcher::new(codebase_path.to_path_buf()));
            }
            Err(e) => {
                // Error finding git root, but continue with current directory
                debug!("Error finding git root ({}), Git HEAD watcher disabled", e);
                return Ok(GitHeadWatcher::new(codebase_path.to_path_buf()));
            }
        };

        debug!("Git repository root: {}", git_root.display());
        Ok(GitHeadWatcher::new(git_root))
    }

    /// Create a new index manager WITHOUT performing incremental refresh.
    ///
    /// Use this when the caller has already performed the refresh (e.g., MCP server).
    /// This avoids FTS lock conflicts by allowing the caller to control when the
    /// refresh happens relative to SharedStores creation.
    ///
    /// # Arguments
    /// * `codebase_path` - Path to the codebase to index
    /// * `stores` - Shared stores for concurrent access (created by caller)
    pub async fn new_without_refresh<P: AsRef<Path>>(
        codebase_path: P,
        stores: Arc<SharedStores>,
    ) -> Result<Self> {
        let path_buf = codebase_path.as_ref().to_path_buf();
        let db_path = path_buf.join(DB_DIR_NAME);

        info!(
            "🔍 Initializing index manager (no refresh) for: {}",
            path_buf.display()
        );

        // Check if index exists
        let needs_initial = Self::needs_initial_indexing(&path_buf).await?;

        if needs_initial {
            // Index doesn't exist - ERROR, don't auto-create
            let error_msg = format!(
                "❌ No index found for: {}\n\n\
                 💡 To create an index, run one of these commands:\n\
                 • For local index:  codesearch index add\n\
                 • For global index: codesearch index add -g\n\n\
                 Then start the server again.",
                path_buf.display()
            );
            return Err(anyhow::anyhow!(error_msg));
        }

        // Create file watcher (but don't start it yet)
        debug!("👀 Creating file watcher...");
        let watcher = FileWatcher::new(path_buf.clone());
        let watcher = Arc::new(Mutex::new(watcher));

        // Create Git HEAD watcher for branch change detection
        debug!("🔀 Creating Git HEAD watcher...");
        let git_head_watcher = Self::find_and_create_git_head_watcher(&path_buf)?;

        info!("✅ Index manager initialized successfully (refresh skipped)");

        Ok(Self {
            codebase_path: path_buf,
            db_path,
            watcher,
            git_head_watcher: Some(git_head_watcher),
            stores,
        })
    }

    /// Perform incremental refresh using shared stores.
    ///
    /// This checks for changed/deleted files since last index and updates
    /// the index accordingly. Uses the shared stores to avoid lock conflicts.
    pub async fn perform_incremental_refresh_with_stores(
        codebase_path: &Path,
        db_path: &Path,
        stores: &SharedStores,
    ) -> Result<()> {
        use crate::cache::FileMetaStore;
        use crate::chunker::SemanticChunker;
        use crate::embed::EmbeddingService;
        use crate::file::FileWalker;

        info!("🔄 Performing incremental refresh with shared stores...");
        let start = std::time::Instant::now();

        // Read model metadata
        let metadata_path = db_path.join("metadata.json");
        let (model_name, dimensions) = if metadata_path.exists() {
            let content = std::fs::read_to_string(&metadata_path)?;
            let json: serde_json::Value = serde_json::from_str(&content)?;
            let model = json
                .get("model_short_name")
                .and_then(|v| v.as_str())
                .unwrap_or("minilm-l6-q");
            let dims = json
                .get("dimensions")
                .and_then(|v| v.as_u64())
                .unwrap_or(384) as usize;
            (model.to_string(), dims)
        } else {
            return Err(anyhow::anyhow!("No metadata.json found in database"));
        };

        // Load FileMetaStore
        let mut file_meta_store = FileMetaStore::load_or_create(db_path, &model_name, dimensions)?;

        // Walk files
        let walker = FileWalker::new(codebase_path.to_path_buf());
        let (files, _stats) = walker.walk()?;

        // Find changed and deleted files
        let mut changed_files = Vec::new();
        let mut unchanged_count = 0;

        for file in &files {
            let (needs_reindex, _old_chunk_ids) = file_meta_store.check_file(&file.path)?;
            if needs_reindex {
                changed_files.push(file.clone());
                debug!("📝 File changed: {}", file.path.display());
            } else {
                unchanged_count += 1;
            }
        }

        // Find deleted files
        let deleted_files = file_meta_store.find_deleted_files();

        info!(
            "   Unchanged: {}, Changed: {}, Deleted: {}",
            unchanged_count,
            changed_files.len(),
            deleted_files.len()
        );

        // If no changes, we're done
        if changed_files.is_empty() && deleted_files.is_empty() {
            info!("✅ Index is up to date!");
            return Ok(());
        }

        // Delete chunks for deleted files
        for (file_path, chunk_ids) in &deleted_files {
            if !chunk_ids.is_empty() {
                debug!("🗑️  Deleting {} chunks for: {}", chunk_ids.len(), file_path);

                // Delete from vector store
                {
                    let mut store = stores.vector_store.write().await;
                    store.delete_chunks(chunk_ids)?;
                }

                // Delete from FTS
                {
                    let mut fts_store = stores.fts_store.write().await;
                    for chunk_id in chunk_ids {
                        fts_store.delete_chunk(*chunk_id)?;
                    }
                }
            }
            file_meta_store.remove_file(Path::new(file_path));
        }

        // Delete old chunks for changed files
        for file in &changed_files {
            let (_, old_chunk_ids) = file_meta_store.check_file(&file.path)?;
            if !old_chunk_ids.is_empty() {
                debug!(
                    "🔄 Deleting {} old chunks for: {}",
                    old_chunk_ids.len(),
                    file.path.display()
                );

                // Delete from vector store
                {
                    let mut store = stores.vector_store.write().await;
                    store.delete_chunks(&old_chunk_ids)?;
                }

                // Delete from FTS
                {
                    let mut fts_store = stores.fts_store.write().await;
                    for chunk_id in &old_chunk_ids {
                        fts_store.delete_chunk(*chunk_id)?;
                    }
                }
            }
        }

        // Commit FTS deletions
        {
            let mut fts_store = stores.fts_store.write().await;
            fts_store.commit()?;
        }

        // Chunk changed files
        if !changed_files.is_empty() {
            info!("🔄 Processing {} changed files...", changed_files.len());

            let mut chunker = SemanticChunker::new(100, 2000, 10);
            let mut all_chunks = Vec::new();

            for file in &changed_files {
                let content = match std::fs::read_to_string(&file.path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let chunks = chunker.chunk_semantic(file.language, &file.path, &content)?;
                all_chunks.extend(chunks);
            }

            if !all_chunks.is_empty() {
                // Embed chunks
                info!("📦 Embedding {} chunks...", all_chunks.len());
                let cache_dir = crate::constants::get_global_models_cache_dir()?;
                let mut embedding_service = EmbeddingService::with_cache_dir(
                    ModelType::default(),
                    Some(cache_dir.as_path()),
                )?;
                let embedded_chunks = embedding_service.embed_chunks(all_chunks)?;

                // Insert into vector store
                let chunk_ids = {
                    let mut store = stores.vector_store.write().await;
                    let ids = store.insert_chunks_with_ids(embedded_chunks.clone())?;
                    store.build_index()?;
                    ids
                };

                // Insert into FTS
                {
                    let mut fts_store = stores.fts_store.write().await;
                    for (chunk, chunk_id) in embedded_chunks.iter().zip(chunk_ids.iter()) {
                        let path_str = chunk.chunk.path.to_string();
                        let signature = chunk.chunk.signature.as_deref();
                        let kind = format!("{:?}", chunk.chunk.kind);
                        fts_store.add_chunk(
                            *chunk_id,
                            &chunk.chunk.content,
                            &path_str,
                            signature,
                            &kind,
                        )?;
                    }
                    fts_store.commit()?;
                }

                // Update file metadata
                // Group chunks by file path (normalize for consistent lookup)
                let mut chunks_by_file: std::collections::HashMap<String, Vec<u32>> =
                    std::collections::HashMap::new();
                for (chunk, chunk_id) in embedded_chunks.iter().zip(chunk_ids.iter()) {
                    chunks_by_file
                        .entry(normalize_path_str(&chunk.chunk.path))
                        .or_default()
                        .push(*chunk_id);
                }

                for file in &changed_files {
                    let path_str = normalize_path(&file.path);
                    if let Some(ids) = chunks_by_file.get(&path_str) {
                        file_meta_store.update_file(&file.path, ids.clone())?;
                    } else {
                        // File was processed but produced 0 chunks (e.g. minified JS,
                        // empty file). Track it with empty chunk list so it is not
                        // re-processed on every run and doctor doesn't flag it.
                        file_meta_store.update_file(&file.path, vec![])?;
                    }
                }

                info!("✅ Indexed {} chunks", embedded_chunks.len());
            } else {
                // ALL changed files produced 0 chunks — still track them so they
                // are not flagged as unindexed on every subsequent run.
                for file in &changed_files {
                    file_meta_store.update_file(&file.path, vec![])?;
                }
            }
        }

        // Save file metadata
        file_meta_store.save(db_path)?;

        let elapsed = start.elapsed();
        info!(
            "✅ Incremental refresh completed in {:.2}s",
            elapsed.as_secs_f64()
        );

        Ok(())
    }

    /// Start the file system watcher (begin collecting events) without starting the processing loop.
    ///
    /// Call this BEFORE a long-running operation (like incremental refresh) to capture
    /// file changes that happen during that operation. Then call `start_file_watcher()`
    /// afterwards to begin processing the buffered events.
    pub async fn start_watching(&self) -> Result<()> {
        let mut w = self.watcher.lock().await;
        if !w.is_started() {
            w.start(DEFAULT_FSW_DEBOUNCE_MS)?;
            info!("👀 File watcher pre-started (collecting events)");
        }
        Ok(())
    }

    /// Start the background file watcher.
    ///
    /// This is the **second method call** - should be called after `new()`.
    /// Spawns a background task that watches for file changes and refreshes the index.
    ///
    /// # Arguments
    /// * `cancel_token` - Cancellation token for graceful shutdown
    ///
    /// # Returns
    /// * `Result<()>` - Success or error
    ///
    /// # Behavior
    /// - Spawns a detached background task
    /// - Watches for file modifications, deletions, and renames
    /// - **Batches events** to avoid overhead with rapid changes
    /// - Flushes batch when no new events for FSW_BATCH_FLUSH_MS
    /// - Logs all file system events and refresh operations
    /// - Continues running even if individual refresh operations fail
    /// - Stops gracefully when the cancellation token is cancelled
    pub async fn start_file_watcher(&self, cancel_token: CancellationToken) -> Result<()> {
        let path = self.codebase_path.clone();
        let db_path = self.db_path.clone();
        let watcher = self.watcher.clone();
        let stores = self.stores.clone();
        let git_head_watcher = self.git_head_watcher.clone();

        info!("🚀 Starting background file watcher...");

        // Spawn background task
        tokio::spawn(async move {
            info!("👀 File watcher task started for: {}", path.display());

            // Start the watcher inside the task (if not already started by start_watching)
            {
                let mut w = watcher.lock().await;
                if !w.is_started() {
                    if let Err(e) = w.start(DEFAULT_FSW_DEBOUNCE_MS) {
                        error!("❌ Failed to start file watcher: {}", e);
                        return;
                    }
                } else {
                    debug!("👀 File watcher already started (pre-started), skipping init");
                }
            }

            // Event buffers - use HashSet to deduplicate
            let mut files_to_index: HashSet<PathBuf> = HashSet::new();
            let mut files_to_remove: HashSet<PathBuf> = HashSet::new();
            let mut last_event_time = std::time::Instant::now();
            let flush_duration = std::time::Duration::from_millis(FSW_BATCH_FLUSH_MS);

            loop {
                // Check if shutdown was requested
                if cancel_token.is_cancelled() {
                    info!("🛑 File watcher received shutdown signal, stopping...");
                    break;
                }

                // Check for branch changes using GitHeadWatcher
                if let Some(watcher) = &git_head_watcher {
                    if let Ok(branch_changed) = watcher.check().await {
                        if branch_changed.is_some() {
                            info!("🔀 Git branch changed, triggering full incremental refresh...");
                            // Perform a real incremental refresh: walk filesystem,
                            // detect changed/deleted files, clean stale chunks, re-index
                            if let Err(e) = Self::refresh_index_with_stores(
                                &path,
                                &db_path,
                                &stores,
                            )
                            .await
                            {
                                error!("❌ Branch change refresh failed: {}", e);
                            }
                            // Clear any buffered file events that arrived during the
                            // branch switch — the full refresh already handled everything
                            files_to_index.clear();
                            files_to_remove.clear();
                        }
                    }
                }

                // Poll for new events
                let events = watcher.lock().await.poll_events();
                let now = std::time::Instant::now();

                if !events.is_empty() {
                    // Log which files are being buffered
                    for event in &events {
                        match event {
                            FileEvent::Modified(p) => debug!("  📄 Buffered: {}", p.display()),
                            FileEvent::Deleted(p) => {
                                debug!("  🗑️  Buffered delete: {}", p.display())
                            }
                            FileEvent::Renamed(old, new) => debug!(
                                "  📝 Buffered rename: {} -> {}",
                                old.display(),
                                new.display()
                            ),
                        }
                    }
                    debug!("📥 Buffered {} file event(s)", events.len());
                    last_event_time = now;

                    // Add events to buffers
                    for event in events {
                        match event {
                            FileEvent::Modified(p) => {
                                // If file was marked for removal, cancel that
                                files_to_remove.remove(&p);
                                files_to_index.insert(p);
                            }
                            FileEvent::Deleted(p) => {
                                // If file was marked for indexing, cancel that
                                files_to_index.remove(&p);
                                files_to_remove.insert(p);
                            }
                            FileEvent::Renamed(old_p, new_p) => {
                                // Remove old path, index new path
                                files_to_index.remove(&old_p);
                                files_to_remove.insert(old_p);
                                files_to_remove.remove(&new_p);
                                files_to_index.insert(new_p);
                            }
                        }
                    }
                }

                // Check if we should flush the buffer
                let has_buffered_events = !files_to_index.is_empty() || !files_to_remove.is_empty();
                let time_since_last_event = now.duration_since(last_event_time);

                if has_buffered_events && time_since_last_event >= flush_duration {
                    // Flush the buffer
                    let to_index: Vec<PathBuf> = files_to_index.drain().collect();
                    let to_remove: Vec<PathBuf> = files_to_remove.drain().collect();

                    info!(
                        "📦 Flushing batch: {} to index, {} to remove",
                        to_index.len(),
                        to_remove.len()
                    );

                    // Process batch using shared stores
                    if let Err(e) = Self::process_batch_with_stores(
                        &path, &db_path, &stores, to_index, to_remove,
                    )
                    .await
                    {
                        error!("❌ Batch processing failed: {}", e);
                    }

                    // Reset timer
                    last_event_time = now;
                }

                // Sleep to avoid busy-waiting, but wake up immediately on shutdown
                tokio::select! {
                    _ = tokio::time::sleep(tokio::time::Duration::from_millis(100)) => {}
                    _ = cancel_token.cancelled() => {
                        info!("🛑 File watcher received shutdown signal during sleep, stopping...");
                        break;
                    }
                }
            }

            info!("✅ File watcher stopped cleanly");
        });

        info!("✅ File watcher background task spawned");

        Ok(())
    }

    /// Process a batch of file events using shared stores.
    /// This is more efficient than processing files one by one.
    async fn process_batch_with_stores(
        codebase_path: &Path,
        db_path: &Path,
        stores: &SharedStores,
        files_to_index: Vec<PathBuf>,
        files_to_remove: Vec<PathBuf>,
    ) -> Result<()> {
        use crate::output::set_quiet;

        let start = std::time::Instant::now();

        // Enable quiet mode during FSW batch processing to suppress verbose embedding output
        set_quiet(true);

        // First, remove deleted files
        for file_path in &files_to_remove {
            debug!("🗑️  Removing: {}", file_path.display());
            if let Err(e) =
                Self::remove_file_from_index_with_stores(codebase_path, db_path, stores, file_path)
                    .await
            {
                warn!("⚠️  Failed to remove {}: {}", file_path.display(), e);
            }

            // Also handle directory deletion: on Windows, rm -rf of a directory may only
            // produce a Remove event for the directory itself, not for individual files.
            // Find all tracked files under this path prefix and remove them too.
            {
                use crate::cache::FileMetaStore;

                // Load FileMetaStore from disk to query tracked files
                let metadata_path = db_path.join("metadata.json");
                if metadata_path.exists() {
                    if let Ok(metadata_str) = std::fs::read_to_string(&metadata_path) {
                        if let Ok(metadata) =
                            serde_json::from_str::<serde_json::Value>(&metadata_str)
                        {
                            let dimensions =
                                metadata["dimensions"].as_u64().unwrap_or(384) as usize;
                            let model_name = metadata["model_short_name"]
                                .as_str()
                                .unwrap_or("minilm-l6-q");

                            if let Ok(file_meta_store) =
                                FileMetaStore::load_or_create(db_path, model_name, dimensions)
                            {
                                // Normalize the directory prefix for consistent matching
                                // (tracked files are normalized to forward slashes)
                                let dir_prefix = normalize_path(file_path);
                                let dir_prefix_slash = if dir_prefix.ends_with('/') {
                                    dir_prefix.clone()
                                } else {
                                    format!("{}/", dir_prefix)
                                };

                                let files_under_dir: Vec<String> = file_meta_store
                                    .tracked_files()
                                    .filter(|f| f.starts_with(&dir_prefix_slash))
                                    .cloned()
                                    .collect();

                                if !files_under_dir.is_empty() {
                                    info!(
                                        "🗑️  Directory deleted: {} ({} files under it)",
                                        file_path.display(),
                                        files_under_dir.len()
                                    );
                                    for tracked_file in &files_under_dir {
                                        let tracked_path = PathBuf::from(tracked_file);
                                        if let Err(e) = Self::remove_file_from_index_with_stores(
                                            codebase_path,
                                            db_path,
                                            stores,
                                            &tracked_path,
                                        )
                                        .await
                                        {
                                            warn!(
                                                "⚠️  Failed to remove {}: {}",
                                                tracked_path.display(),
                                                e
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Rebuild vector index after removals so deleted chunks are excluded from search results.
        // index_single_file_with_stores already calls build_index() per file, but when a batch
        // contains ONLY removals (no additions), the index would never be rebuilt without this.
        if !files_to_remove.is_empty() {
            let mut store = stores.vector_store.write().await;
            store.build_index()?;
        }

        // Then, index modified/new files
        for file_path in &files_to_index {
            debug!("📄 Indexing: {}", file_path.display());
            if let Err(e) = Self::index_single_file(codebase_path, file_path, stores).await {
                warn!("⚠️  Failed to index {}: {}", file_path.display(), e);
            }
        }

        // Disable quiet mode after batch processing is complete
        set_quiet(false);

        let elapsed = start.elapsed();
        info!(
            "✅ Batch complete: {} indexed, {} removed in {:.2}s",
            files_to_index.len(),
            files_to_remove.len(),
            elapsed.as_secs_f64()
        );

        Ok(())
    }

    /// Perform a full incremental refresh using shared stores.
    ///
    /// This is called on git branch changes to ensure the index reflects the
    /// current state of the working tree. Unlike `process_batch_with_stores`
    /// which operates on a known list of changed files, this function:
    ///
    /// 1. Walks the filesystem to discover all current files
    /// 2. Compares each against FileMetaStore to find changed/new files
    /// 3. Uses find_deleted_files() to detect stale entries (ghost files)
    /// 4. Deletes stale chunks from VectorStore + FtsStore
    /// 5. Rebuilds the vector index
    /// 6. Re-indexes changed/new files
    async fn refresh_index_with_stores(
        codebase_path: &Path,
        db_path: &Path,
        stores: &SharedStores,
    ) -> Result<()> {
        use crate::cache::FileMetaStore;
        use crate::file::FileWalker;
        use crate::output::set_quiet;

        let start = std::time::Instant::now();
        set_quiet(true);

        // Phase 1: Discover current files on disk
        let walker = FileWalker::new(codebase_path.to_path_buf());
        let (files, stats) = walker.walk()?;
        info!(
            "🔍 Branch refresh: discovered {} indexable files ({} skipped)",
            files.len(),
            stats.total_files - stats.indexable_files
        );

        // Phase 2: Load file metadata and analyze changes
        let metadata_path = db_path.join("metadata.json");
        if !metadata_path.exists() {
            info!("⚠️ No metadata.json found, skipping branch refresh");
            set_quiet(false);
            return Ok(());
        }
        let metadata_str = std::fs::read_to_string(&metadata_path)?;
        let metadata: serde_json::Value = serde_json::from_str(&metadata_str)?;
        let dimensions = metadata["dimensions"].as_u64().unwrap_or(384) as usize;
        let model_name = metadata["model_short_name"]
            .as_str()
            .unwrap_or("minilm-l6-q");

        let mut file_meta_store =
            FileMetaStore::load_or_create(db_path, model_name, dimensions)?;

        // Find files that need re-indexing (new or content changed)
        let mut files_to_reindex: Vec<PathBuf> = Vec::new();
        let mut chunks_to_delete: Vec<u32> = Vec::new();

        for file_info in &files {
            let (needs_reindex, old_chunk_ids) = file_meta_store.check_file(&file_info.path)?;
            if needs_reindex {
                chunks_to_delete.extend(old_chunk_ids);
                files_to_reindex.push(file_info.path.clone());
            }
        }

        // Find files that were deleted (tracked in metadata but not on disk)
        let deleted_files = file_meta_store.find_deleted_files();

        if files_to_reindex.is_empty() && deleted_files.is_empty() {
            info!("✅ Branch refresh: index is up to date, no changes needed");
            set_quiet(false);
            return Ok(());
        }

        info!(
            "🔍 Branch refresh analysis: {} to re-index, {} stale to remove, {} old chunks to clean",
            files_to_reindex.len(),
            deleted_files.len(),
            chunks_to_delete.len()
        );

        // Phase 3: Collect ALL chunk IDs to delete (changed + deleted files)
        for (_file_path, chunk_ids) in &deleted_files {
            chunks_to_delete.extend(chunk_ids);
        }

        // Batch-delete all stale chunks from both stores
        if !chunks_to_delete.is_empty() {
            {
                let mut vstore = stores.vector_store.write().await;
                vstore.delete_chunks(&chunks_to_delete)?;
            }
            {
                let mut fstore = stores.fts_store.write().await;
                for &chunk_id in &chunks_to_delete {
                    fstore.delete_chunk(chunk_id)?;
                }
                fstore.commit()?;
            }
        }

        // Remove deleted files from FileMetaStore
        let deleted_count = deleted_files.len();
        for (file_path, _chunk_ids) in &deleted_files {
            file_meta_store.remove_file(std::path::Path::new(file_path));
        }

        // Save metadata after deletions (before re-indexing, since
        // index_single_file loads its own fresh copy per file)
        file_meta_store.save(db_path)?;

        // Rebuild vector index after all chunk deletions
        {
            let mut vstore = stores.vector_store.write().await;
            vstore.build_index()?;
        }

        // Phase 4: Re-index changed/new files
        let reindex_count = files_to_reindex.len();
        for file_path in &files_to_reindex {
            if let Err(e) = Self::index_single_file(codebase_path, file_path, stores).await {
                warn!("⚠️  Failed to re-index {}: {}", file_path.display(), e);
            }
        }

        set_quiet(false);

        let elapsed = start.elapsed();
        info!(
            "✅ Branch refresh complete: {} re-indexed, {} stale removed in {:.2}s",
            reindex_count,
            deleted_count,
            elapsed.as_secs_f64()
        );

        Ok(())
    }

    /// Check if initial indexing is needed.
    async fn needs_initial_indexing(path: &Path) -> Result<bool> {
        // Check for DB_DIR_NAME directory (the only correct path)
        let db_path = path.join(DB_DIR_NAME);
        let meta_db_path = db_path.join(FILE_META_DB_NAME);

        if !meta_db_path.exists() {
            debug!(
                "📂 File metadata database not found at: {}",
                meta_db_path.display()
            );
            return Ok(true);
        }

        // Check if database is empty or corrupted
        // This is a simplified check - in production you might want more sophisticated checks
        Ok(false)
    }

    /// Perform initial full indexing.
    #[allow(dead_code)]
    async fn perform_initial_indexing(path: &Path) -> Result<()> {
        info!("🔨 Performing full indexing (this may take a while)...");
        let start = std::time::Instant::now();

        // Call the index function from the parent module
        // Parameters: path, dry_run, force, global, model
        super::index(
            Some(path.to_path_buf()),
            false,
            false,
            false,
            None,
            CancellationToken::new(),
        )
        .await?;

        let elapsed = start.elapsed();
        info!(
            "✅ Full indexing completed in {:.2}s",
            elapsed.as_secs_f64()
        );

        Ok(())
    }

    /// Perform incremental index refresh.
    async fn perform_incremental_refresh(path: &Path) -> Result<()> {
        info!("🔄 Performing incremental index refresh...");
        let start = std::time::Instant::now();

        // Call the quiet index function from the parent module (no CLI output)
        // For incremental refresh, we use force=false which enables incremental mode
        super::index_quiet(Some(path.to_path_buf()), false, CancellationToken::new()).await?;

        let elapsed = start.elapsed();
        info!(
            "✅ Incremental refresh completed in {:.2}s",
            elapsed.as_secs_f64()
        );

        Ok(())
    }

    /// Index a single file (for FSW events).
    /// This is much faster than a full incremental refresh.
    async fn index_single_file(
        codebase_path: &Path,
        file_path: &Path,
        stores: &SharedStores,
    ) -> Result<()> {
        use crate::cache::FileMetaStore;
        use crate::chunker::{Chunker, SemanticChunker};
        use crate::embed::EmbeddingService;
        use crate::file::Language;

        let db_path = codebase_path.join(DB_DIR_NAME);

        // Check if file exists and is indexable
        if !file_path.exists() {
            debug!("File no longer exists, skipping: {}", file_path.display());
            return Ok(());
        }

        let language = Language::from_path(file_path);
        if !language.is_indexable() {
            debug!("File not indexable, skipping: {}", file_path.display());
            return Ok(());
        }

        // Read file content
        let content = match std::fs::read_to_string(file_path) {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to read file {}: {}", file_path.display(), e);
                return Ok(());
            }
        };

        // Chunk the file
        let chunker = SemanticChunker::new(100, 4000, 2);
        let chunks = chunker.chunk_file(file_path, &content)?;

        if chunks.is_empty() {
            debug!("No chunks created for file: {}", file_path.display());
            return Ok(());
        }

        debug!(
            "Created {} chunks for file: {}",
            chunks.len(),
            file_path.display()
        );

        // Generate embeddings
        let cache_dir = crate::constants::get_global_models_cache_dir()?;
        let mut embedding_service =
            EmbeddingService::with_cache_dir(ModelType::default(), Some(cache_dir.as_path()))?;
        let embedded_chunks = embedding_service.embed_chunks(chunks)?;

        // Load metadata to get dimensions
        let metadata_path = db_path.join("metadata.json");
        let metadata: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&metadata_path)?)?;
        let dimensions = metadata["dimensions"].as_u64().unwrap_or(384) as usize;
        let model_name = metadata["model_short_name"]
            .as_str()
            .unwrap_or("minilm-l6-q");

        // Use shared stores with write lock
        let chunk_ids = {
            let mut store = stores.vector_store.write().await;
            let chunk_ids = store.insert_chunks_with_ids(embedded_chunks.clone())?;
            // Rebuild the vector index after inserting new chunks
            store.build_index()?;
            chunk_ids
        };

        // Add to FTS with write lock
        {
            let mut fts_store = stores.fts_store.write().await;
            for (chunk, chunk_id) in embedded_chunks.iter().zip(chunk_ids.iter()) {
                let path_str = chunk.chunk.path.to_string();
                let signature = chunk.chunk.signature.as_deref();
                let kind = format!("{:?}", chunk.chunk.kind);
                fts_store.add_chunk(
                    *chunk_id,
                    &chunk.chunk.content,
                    &path_str,
                    signature,
                    &kind,
                )?;
            }
            fts_store.commit()?;
        }

        // Update file metadata (separate store, not shared)
        let mut file_meta_store = FileMetaStore::load_or_create(&db_path, model_name, dimensions)?;
        file_meta_store.update_file(file_path, chunk_ids)?;
        file_meta_store.save(&db_path)?;

        info!(
            "✅ Indexed {} ({} chunks)",
            file_path.display(),
            embedded_chunks.len()
        );

        Ok(())
    }

    /// Remove a file from the index using shared stores (for FSW delete events).
    /// This version uses the shared stores to avoid LMDB conflicts.
    async fn remove_file_from_index_with_stores(
        _codebase_path: &Path,
        db_path: &Path,
        stores: &SharedStores,
        file_path: &Path,
    ) -> Result<()> {
        use crate::cache::FileMetaStore;

        // Load metadata to get dimensions and model
        let metadata_path = db_path.join("metadata.json");
        if !metadata_path.exists() {
            debug!("No metadata found, skipping removal");
            return Ok(());
        }
        let metadata: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&metadata_path)?)?;
        let dimensions = metadata["dimensions"].as_u64().unwrap_or(384) as usize;
        let model_name = metadata["model_short_name"]
            .as_str()
            .unwrap_or("minilm-l6-q");

        // Load file metadata to get chunk IDs
        let mut file_meta_store = FileMetaStore::load_or_create(db_path, model_name, dimensions)?;

        // Get chunk IDs from file metadata directly (not check_file which reads from disk)
        // The file is already deleted, so we can't read mtime/size/hash
        let meta = file_meta_store.remove_file(file_path);
        let chunk_ids = match meta {
            Some(m) if !m.chunk_ids.is_empty() => m.chunk_ids,
            Some(_) => {
                debug!("No chunks to remove for file: {}", file_path.display());
                file_meta_store.save(db_path)?;
                return Ok(());
            }
            None => {
                debug!("No metadata found for file: {}", file_path.display());
                return Ok(());
            }
        };

        debug!(
            "Removing {} chunks for file: {}",
            chunk_ids.len(),
            file_path.display()
        );

        // Delete chunks from vector store with write lock
        {
            let mut store = stores.vector_store.write().await;
            for chunk_id in &chunk_ids {
                store.delete_chunks(&[*chunk_id])?;
            }
        }

        // Delete from FTS with write lock
        {
            let mut fts_store = stores.fts_store.write().await;
            for chunk_id in &chunk_ids {
                fts_store.delete_chunk(*chunk_id)?;
            }
            fts_store.commit()?;
        }

        // Save file metadata (remove_file was already called above)
        file_meta_store.save(db_path)?;

        info!(
            "✅ Removed {} chunks for {}",
            chunk_ids.len(),
            file_path.display()
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::FileMetaStore;
    use tempfile::tempdir;

    /// Helper: create metadata.json in db_path with given dimensions
    fn create_metadata_json(db_path: &Path, dimensions: usize) {
        let metadata = serde_json::json!({
            "dimensions": dimensions,
            "model_short_name": "test-model"
        });
        std::fs::write(
            db_path.join("metadata.json"),
            serde_json::to_string_pretty(&metadata).unwrap(),
        )
        .unwrap();
    }

    /// Helper: create writable SharedStores for testing (no writer lock)
    async fn create_test_stores(db_path: &Path, dimensions: usize) -> SharedStores {
        use crate::fts::FtsStore;
        use crate::vectordb::VectorStore;

        SharedStores {
            vector_store: Arc::new(RwLock::new(
                VectorStore::new(db_path, dimensions).unwrap(),
            )),
            fts_store: Arc::new(RwLock::new(
                FtsStore::new_with_writer(db_path).unwrap(),
            )),
            writer_lock: None,
            readonly: false,
        }
    }

    #[tokio::test]
    async fn test_refresh_no_metadata_early_return() {
        // When metadata.json doesn't exist, refresh should return Ok early
        let temp = tempdir().unwrap();
        let codebase_path = temp.path().join("codebase");
        let db_path = temp.path().join("db");
        std::fs::create_dir_all(&codebase_path).unwrap();
        std::fs::create_dir_all(&db_path).unwrap();

        // Don't create metadata.json
        let stores = create_test_stores(&db_path, 4).await;

        let result = IndexManager::refresh_index_with_stores(
            &codebase_path,
            &db_path,
            &stores,
        )
        .await;

        assert!(result.is_ok(), "Should return Ok when no metadata.json exists");
    }

    #[tokio::test]
    async fn test_refresh_removes_ghost_file_entries() {
        // Ghost files (tracked in FileMetaStore but not on disk) should be cleaned up
        let temp = tempdir().unwrap();
        let codebase_path = temp.path().join("codebase");
        let db_path = temp.path().join("db");
        std::fs::create_dir_all(&codebase_path).unwrap();
        std::fs::create_dir_all(&db_path).unwrap();

        create_metadata_json(&db_path, 4);

        // Create a ghost file temporarily so update_file can read its metadata
        let ghost_file = codebase_path.join("ghost.rs");
        std::fs::write(&ghost_file, "fn ghost() {}").unwrap();

        // Track the ghost file in FileMetaStore
        let mut file_meta = FileMetaStore::new("test-model".to_string(), 4);
        file_meta
            .update_file(&ghost_file, vec![100, 101])
            .unwrap();
        file_meta.save(&db_path).unwrap();

        // Now delete the ghost file from disk — simulates branch switch
        std::fs::remove_file(&ghost_file).unwrap();

        // Verify precondition: ghost file IS tracked but NOT on disk
        let deleted_before = file_meta.find_deleted_files();
        assert_eq!(
            deleted_before.len(),
            1,
            "Should find one ghost file before refresh"
        );
        assert_eq!(
            deleted_before[0].1,
            vec![100, 101],
            "Ghost file should have chunk_ids [100, 101]"
        );

        // Create SharedStores (empty — ghost chunk IDs won't exist in store,
        // but delete_chunks handles missing IDs gracefully)
        let stores = create_test_stores(&db_path, 4).await;

        // Run the refresh
        let result = IndexManager::refresh_index_with_stores(
            &codebase_path,
            &db_path,
            &stores,
        )
        .await;

        assert!(result.is_ok(), "Refresh should succeed: {:?}", result);

        // Verify: reload FileMetaStore and confirm ghost entry is gone
        let reloaded = FileMetaStore::load_or_create(&db_path, "test-model", 4).unwrap();
        let deleted_after = reloaded.find_deleted_files();
        assert!(
            deleted_after.is_empty(),
            "Ghost file should have been removed from FileMetaStore after refresh, found: {:?}",
            deleted_after
        );
    }

    #[tokio::test]
    async fn test_refresh_removes_multiple_ghost_files() {
        // Multiple ghost files should all be cleaned up in one refresh
        let temp = tempdir().unwrap();
        let codebase_path = temp.path().join("codebase");
        let db_path = temp.path().join("db");
        std::fs::create_dir_all(&codebase_path).unwrap();
        std::fs::create_dir_all(&db_path).unwrap();

        create_metadata_json(&db_path, 4);

        // Create ghost files temporarily
        let ghost1 = codebase_path.join("ghost1.rs");
        let ghost2 = codebase_path.join("ghost2.rs");
        let ghost3 = codebase_path.join("ghost3.rs");
        std::fs::write(&ghost1, "fn g1() {}").unwrap();
        std::fs::write(&ghost2, "fn g2() {}").unwrap();
        std::fs::write(&ghost3, "fn g3() {}").unwrap();

        // Track all ghost files
        let mut file_meta = FileMetaStore::new("test-model".to_string(), 4);
        file_meta.update_file(&ghost1, vec![10, 11]).unwrap();
        file_meta.update_file(&ghost2, vec![20, 21, 22]).unwrap();
        file_meta.update_file(&ghost3, vec![30]).unwrap();
        file_meta.save(&db_path).unwrap();

        // Delete all ghost files
        std::fs::remove_file(&ghost1).unwrap();
        std::fs::remove_file(&ghost2).unwrap();
        std::fs::remove_file(&ghost3).unwrap();

        // Verify precondition
        let deleted_before = file_meta.find_deleted_files();
        assert_eq!(
            deleted_before.len(),
            3,
            "Should find 3 ghost files before refresh"
        );

        let stores = create_test_stores(&db_path, 4).await;

        let result = IndexManager::refresh_index_with_stores(
            &codebase_path,
            &db_path,
            &stores,
        )
        .await;

        assert!(result.is_ok(), "Refresh should succeed: {:?}", result);

        // All ghost entries should be removed
        let reloaded = FileMetaStore::load_or_create(&db_path, "test-model", 4).unwrap();
        let deleted_after = reloaded.find_deleted_files();
        assert!(
            deleted_after.is_empty(),
            "All 3 ghost files should be removed, found: {:?}",
            deleted_after
        );
    }

    #[tokio::test]
    async fn test_refresh_preserves_valid_entries() {
        // Files that exist on disk and match metadata should NOT be touched
        let temp = tempdir().unwrap();
        let codebase_path = temp.path().join("codebase");
        let db_path = temp.path().join("db");
        std::fs::create_dir_all(&codebase_path).unwrap();
        std::fs::create_dir_all(&db_path).unwrap();

        create_metadata_json(&db_path, 4);

        // Create a real file on disk
        let real_file = codebase_path.join("main.rs");
        std::fs::write(&real_file, "fn main() { println!(\"hello\"); }").unwrap();

        // Track it in FileMetaStore (update_file reads mtime/size/hash)
        let mut file_meta = FileMetaStore::new("test-model".to_string(), 4);
        file_meta.update_file(&real_file, vec![1, 2]).unwrap();
        file_meta.save(&db_path).unwrap();

        let stores = create_test_stores(&db_path, 4).await;

        let result = IndexManager::refresh_index_with_stores(
            &codebase_path,
            &db_path,
            &stores,
        )
        .await;

        assert!(result.is_ok(), "Refresh should succeed: {:?}", result);

        // Verify: real file entry should still be in FileMetaStore
        let reloaded = FileMetaStore::load_or_create(&db_path, "test-model", 4).unwrap();
        let deleted = reloaded.find_deleted_files();
        assert!(
            deleted.is_empty(),
            "Real file should NOT be removed from FileMetaStore"
        );
    }

    #[tokio::test]
    async fn test_refresh_mixed_ghost_and_real_files() {
        // Ghost files should be removed while real files are preserved
        let temp = tempdir().unwrap();
        let codebase_path = temp.path().join("codebase");
        let db_path = temp.path().join("db");
        std::fs::create_dir_all(&codebase_path).unwrap();
        std::fs::create_dir_all(&db_path).unwrap();

        create_metadata_json(&db_path, 4);

        // Create both real and ghost files
        let real_file = codebase_path.join("real.rs");
        let ghost_file = codebase_path.join("ghost.rs");
        std::fs::write(&real_file, "fn real() { 42 }").unwrap();
        std::fs::write(&ghost_file, "fn ghost() { 99 }").unwrap();

        // Track both files
        let mut file_meta = FileMetaStore::new("test-model".to_string(), 4);
        file_meta.update_file(&real_file, vec![1, 2]).unwrap();
        file_meta
            .update_file(&ghost_file, vec![3, 4, 5])
            .unwrap();
        file_meta.save(&db_path).unwrap();

        // Delete ghost file — simulates branch switch removing it
        std::fs::remove_file(&ghost_file).unwrap();

        let stores = create_test_stores(&db_path, 4).await;

        let result = IndexManager::refresh_index_with_stores(
            &codebase_path,
            &db_path,
            &stores,
        )
        .await;

        assert!(result.is_ok(), "Refresh should succeed: {:?}", result);

        // Verify: ghost is removed, real is preserved
        let reloaded = FileMetaStore::load_or_create(&db_path, "test-model", 4).unwrap();
        let deleted = reloaded.find_deleted_files();
        assert!(
            deleted.is_empty(),
            "Ghost entry should be removed, real should remain. Found deleted: {:?}",
            deleted
        );

        // Verify the real file is still tracked by checking it doesn't need reindex
        let (needs_reindex, _chunk_ids) = reloaded.check_file(&real_file).unwrap();
        assert!(
            !needs_reindex,
            "Real file should still be tracked and up-to-date in FileMetaStore"
        );
    }

    #[tokio::test]
    async fn test_refresh_empty_codebase_cleans_all_stale() {
        // If codebase is empty (all files deleted), ALL tracked entries become ghosts
        let temp = tempdir().unwrap();
        let codebase_path = temp.path().join("codebase");
        let db_path = temp.path().join("db");
        std::fs::create_dir_all(&codebase_path).unwrap();
        std::fs::create_dir_all(&db_path).unwrap();

        create_metadata_json(&db_path, 4);

        // Create files temporarily to get them tracked
        let file1 = codebase_path.join("lib.rs");
        let file2 = codebase_path.join("util.rs");
        std::fs::write(&file1, "pub fn lib_fn() {}").unwrap();
        std::fs::write(&file2, "pub fn util_fn() {}").unwrap();

        let mut file_meta = FileMetaStore::new("test-model".to_string(), 4);
        file_meta.update_file(&file1, vec![1, 2, 3]).unwrap();
        file_meta.update_file(&file2, vec![4, 5]).unwrap();
        file_meta.save(&db_path).unwrap();

        // Delete ALL files — simulates switching to a branch with no source
        std::fs::remove_file(&file1).unwrap();
        std::fs::remove_file(&file2).unwrap();

        let stores = create_test_stores(&db_path, 4).await;

        let result = IndexManager::refresh_index_with_stores(
            &codebase_path,
            &db_path,
            &stores,
        )
        .await;

        assert!(result.is_ok(), "Refresh should succeed: {:?}", result);

        // All entries should be cleaned
        let reloaded = FileMetaStore::load_or_create(&db_path, "test-model", 4).unwrap();
        let deleted = reloaded.find_deleted_files();
        assert!(
            deleted.is_empty(),
            "All stale entries should be removed"
        );
    }
}
