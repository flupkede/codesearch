//! Shared index stores and writer-lock management.
//!
//! Store-open mechanics only: opening the vector/FTS stores, the cross-process
//! writer lock and the heavy-job gate. Index lifecycle (add/remove/index/prune/
//! stats/clear) stays in the CLI crate.

use crate::constants::WRITER_LOCK_FILE;
use crate::fts::FtsStore;
use crate::vectordb::VectorStore;
use anyhow::Result;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

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

    // Ensure the database directory exists before placing the lock file inside it.
    // For a brand-new repo (e.g. auto-register via POST /repos) the `.codesearch.db`
    // directory does not exist yet. Without this, opening the lock file below fails
    // with "path not found", which we'd misreport to the caller as
    // "Database is locked by another process" — causing a spurious 500 on register.
    // (SharedStores::new also creates this directory so it can surface genuine I/O
    // errors distinctly; this call keeps acquire_writer_lock correct for any other
    // caller. create_dir_all is idempotent, so the duplication is harmless.)
    if let Err(e) = std::fs::create_dir_all(db_path) {
        warn!(
            "Failed to create database directory {}: {}",
            db_path.display(),
            e
        );
        return None;
    }

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
    /// Lock file handle (Some = we have writer lock, None = readonly mode).
    /// Pub so test helpers can assemble stores without taking the lock.
    pub writer_lock: Option<File>,
    /// Whether this instance is in readonly mode
    pub readonly: bool,
    /// Counter for number of file changes processed (indexed + removed) since serve start.
    /// Incremented by FSW batches and incremental refreshes. Read by TUI/dashboard.
    pub changes_count: std::sync::atomic::AtomicU64,
    /// Process-wide heavy-job gate. `Some` in serve mode (attached by
    /// `ServeState::try_open_stores`), `None` in standalone CLI/MCP processes.
    /// Every whole-repo refresh/reindex/build takes one permit from it; see
    /// [`crate::limits`].
    pub job_gate: Option<Arc<crate::limits::JobGate>>,
}

/// Mark the database directory hidden on Windows (the ".git" convention):
/// the Search indexer skips hidden directories, and its scanner holding a
/// freshly created Tantivy segment file is exactly the transient commit
/// race `IndexManager::force_reindex_with_stores` retries around.
/// Best-effort and idempotent — a missing or failing attrib never blocks
/// indexing.
#[cfg(windows)]
fn hide_db_dir_on_windows(db_path: &Path) {
    let _ = std::process::Command::new("attrib")
        .arg("+H")
        .arg(db_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    // The fts/ subdir is created lazily by the tantivy writer and does not
    // inherit the parent's hidden attribute; hide it too so no indexer ever
    // crawls fresh segment files. Best-effort, same as the parent.
    let _ = std::process::Command::new("attrib")
        .arg("+H")
        .arg(db_path.join("fts"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(not(windows))]
fn hide_db_dir_on_windows(_db_path: &Path) {}

impl SharedStores {
    /// Attach a process-wide heavy-job gate to these stores (serve mode).
    pub fn with_job_gate(mut self, gate: Arc<crate::limits::JobGate>) -> Self {
        self.job_gate = Some(gate);
        self
    }

    /// Acquire one heavy-job permit, or `None` when no gate is attached
    /// (standalone processes keep their previous unbounded behaviour).
    ///
    /// Hold the returned permit for the whole job: that is what bounds how
    /// many repos chunk/embed/build at once. Queries never take this permit.
    pub async fn acquire_job_permit(&self) -> Option<crate::limits::JobPermit> {
        match self.job_gate.as_ref() {
            Some(gate) => Some(gate.acquire().await),
            None => None,
        }
    }

    /// Cancellation-aware variant of [`Self::acquire_job_permit`].
    ///
    /// Returns [`crate::limits::JobAcquire::Cancelled`] when `cancel_token`
    /// fires (or already fired) before a slot is granted, so the caller can
    /// abort the job promptly and release its stores instead of parking on
    /// the gate for the permit holder's whole run.
    pub async fn acquire_job_permit_or_cancel(
        &self,
        cancel_token: &CancellationToken,
    ) -> crate::limits::JobAcquire {
        match self.job_gate.as_ref() {
            Some(gate) => match gate.acquire_or_cancel(cancel_token).await {
                Some(permit) => crate::limits::JobAcquire::Ready(Some(permit)),
                None => crate::limits::JobAcquire::Cancelled,
            },
            None => {
                if cancel_token.is_cancelled() {
                    crate::limits::JobAcquire::Cancelled
                } else {
                    crate::limits::JobAcquire::Ready(None)
                }
            }
        }
    }

    /// Create new shared stores from the database path (read-write mode).
    ///
    /// This acquires a writer lock. If another process already has the lock,
    /// this will fail with an error.
    pub fn new(db_path: &Path, dimensions: usize) -> Result<Self> {
        use anyhow::Context;

        // Ensure the database directory exists first, propagating any genuine I/O
        // error (e.g. permission denied) as itself. `acquire_writer_lock` also
        // creates the directory defensively, but doing it here lets us distinguish
        // a real filesystem failure from a held lock: after this succeeds, a `None`
        // from `acquire_writer_lock` unambiguously means "locked by another process".
        std::fs::create_dir_all(db_path).with_context(|| {
            format!("Failed to create database directory {}", db_path.display())
        })?;
        // The dot in ".codesearch.db" means nothing to Windows: the Search
        // indexer crawls the folder like any other, and its scanner holding
        // a freshly created Tantivy segment file is exactly the transient
        // commit race `force_reindex_with_stores` retries around. The
        // indexer skips hidden directories (the ".git" convention), so mark
        // the folder hidden — idempotent, best-effort.
        hide_db_dir_on_windows(db_path);

        // Try to acquire writer lock
        let lock = acquire_writer_lock(db_path);
        if lock.is_none() {
            return Err(anyhow::anyhow!(
                "Database is locked by another process. Use new_readonly() instead."
            ));
        }

        let vector_store = VectorStore::new(db_path, dimensions)?;
        // The FTS writer stays lazy on purpose: creating it eagerly parks
        // tantivy's writer-pipeline threads (workers + segment updater + merge
        // pool) for as long as serve holds the store — with one store per warm
        // repo that was thousands of resident threads on a large pool. It
        // materializes on the first actual write (FtsStore::ensure_writer).
        let fts_store = FtsStore::new(db_path)?;

        info!("📦 SharedStores created in read-write mode");

        Ok(Self {
            vector_store: Arc::new(RwLock::new(vector_store)),
            fts_store: Arc::new(RwLock::new(fts_store)),
            writer_lock: lock,
            readonly: false,
            changes_count: std::sync::atomic::AtomicU64::new(0),
            job_gate: None,
        })
    }

    /// Create shared stores in readonly mode (for secondary instances).
    ///
    /// This does not acquire any locks and cannot write to the database.
    /// File watching is not supported in readonly mode.
    pub fn new_readonly(db_path: &Path, dimensions: usize) -> Result<Self> {
        let vector_store = VectorStore::open_readonly(db_path, dimensions)?;
        // The OnCommitWithDelay watcher is deliberate here: by construction
        // another process holds the writer lock and may commit at any time,
        // and this reader must pick those commits up without a reopen.
        let fts_store = FtsStore::new_readonly(db_path)?;

        info!("📦 SharedStores created in readonly mode");

        Ok(Self {
            vector_store: Arc::new(RwLock::new(vector_store)),
            fts_store: Arc::new(RwLock::new(fts_store)),
            writer_lock: None,
            readonly: true,
            changes_count: std::sync::atomic::AtomicU64::new(0),
            job_gate: None,
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
