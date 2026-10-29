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
use crate::constants::{
    DB_DIR_NAME, DEFAULT_FSW_DEBOUNCE_MS, FILE_META_DB_NAME, LANG_CSHARP, LANG_TYPESCRIPT,
    SCIP_CSHARP_DEBOUNCE_MS, SCIP_TYPESCRIPT_DEBOUNCE_MS, WRITER_LOCK_FILE,
};
use crate::embed::{EmbeddingServicePool, ModelType};
use crate::fts::FtsStore;
use crate::symbols::{RebuildScope, SymbolIndexer, SymbolIndexerRegistry};
use crate::vectordb::{ChunkStore, VectorStore};
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

/// Signal sent to the serve layer about a watcher-triggered symbol rebuild.
///
/// Unlike the old two-argument `(success, error)` callback, this carries a
/// `Started` variant so the serve layer can flip the C# indicator to
/// `Indexing` for the *duration* of the rebuild — matching what the
/// serve-side `trigger_symbol_rebuild` already does for the phase-2 /
/// POST-/reindex paths. Without `Started`, a watcher-triggered rebuild only
/// ever reported its terminal state, so the C#-specific indicator never showed
/// "Indexing" while the (35–84s) rebuild was actually running.
#[derive(Debug, PartialEq)]
pub enum SymbolRebuildSignal {
    /// A rebuild is about to run (helper available and project applies).
    Started,
    /// Rebuild finished successfully. Carries the index-level warnings the
    /// rebuild survived (empty = clean) so the serve layer can render a
    /// degraded index red instead of green — the watcher path is the common
    /// path during active development, and a green indicator on an index
    /// with missing cross-project references reads as "safe".
    Succeeded { index_warnings: Vec<String> },
    /// Rebuild failed with the given message.
    Failed(String),
    /// Rebuild was abandoned because `dotnet restore` never finished within
    /// the retry cap: LMDB untouched, the previous index stays live. NOT a
    /// failure — the serve layer renders a warning (yellow C#⚠), not a red
    /// error.
    Abandoned(String),
}

/// Terminal signal for a grouped incremental rebuild: a real failure
/// (`last_error`) wins over an abandonment (`last_abandon`), which wins over
/// success. Pure so the ordering is table-testable — the watcher-path
/// counterpart of the serve layer's `classify_symbol_rebuild_failure`.
pub(crate) fn grouped_rebuild_signal(
    last_error: Option<String>,
    last_abandon: Option<String>,
    index_warnings: Vec<String>,
) -> SymbolRebuildSignal {
    if let Some(msg) = last_error {
        SymbolRebuildSignal::Failed(msg)
    } else if let Some(msg) = last_abandon {
        SymbolRebuildSignal::Abandoned(msg)
    } else {
        SymbolRebuildSignal::Succeeded { index_warnings }
    }
}

/// Callback invoked around each watcher-triggered C# symbol rebuild.
///
/// Called with [`SymbolRebuildSignal::Started`] just before the rebuild runs,
/// then exactly once more with [`SymbolRebuildSignal::Succeeded`],
/// [`SymbolRebuildSignal::Failed`] or [`SymbolRebuildSignal::Abandoned`] when
/// it finishes.
///
/// The serve layer uses this to update `csharp_index_status` / `csharp_index_error`
/// without coupling `IndexManager` to `ServeState`.
pub type CSharpRebuildNotifier = Arc<dyn Fn(SymbolRebuildSignal) + Send + Sync>;

/// Callback to notify the serve layer that text/vector indexing is active or idle.
///
/// Arguments: `(active: bool)` — `true` when indexing starts, `false` when it completes.
///
/// The serve layer uses this to update `active_reindexes` so the TUI shows "Indexing"
/// during file-watcher-triggered refreshes (branch changes, batch flushes).
pub type IndexingStatusCallback = Arc<dyn Fn(bool) + Send + Sync>;

/// Heartbeat fired by long indexing loops (once per batch, plus before each
/// slow phase) so the caller can renew its indexing marker before
/// `MAX_INDEXING_SECS` expires. Large repos' warmups run well past 30 minutes;
/// without renewal the stale-marker eviction drops the marker mid-refresh and
/// the reaper/FSW can then race the live refresh (todo #131 shape).
pub type IndexingHeartbeat = Arc<dyn Fn() + Send + Sync>;

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
    pub vector_store: Arc<RwLock<ChunkStore>>,
    pub fts_store: Arc<RwLock<FtsStore>>,
    /// Lock file handle (Some = we have writer lock, None = readonly mode)
    #[allow(dead_code)]
    writer_lock: Option<File>,
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
/// race [`IndexManager::force_reindex_with_stores`] retries around.
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
    pub async fn open(
        db_path: &Path,
        project_path: &Path,
        model: ModelType,
        dimensions: usize,
        readonly: bool,
    ) -> Result<Self> {
        if crate::vectordb::storage_backend(db_path)? == "local" {
            return if readonly {
                Self::new_readonly(db_path, dimensions)
            } else {
                Self::new(db_path, dimensions)
            };
        }
        std::fs::create_dir_all(db_path)?;
        let writer_lock = if readonly {
            None
        } else {
            Some(
                acquire_writer_lock(db_path)
                    .ok_or_else(|| anyhow::anyhow!("Database is locked by another process"))?,
            )
        };
        let vector_store = ChunkStore::open_for_path(
            db_path,
            project_path,
            model.short_name(),
            dimensions,
            readonly,
        )
        .await?;
        let fts_store = if readonly {
            FtsStore::new_readonly(db_path)?
        } else {
            FtsStore::new(db_path)?
        };
        Ok(Self {
            vector_store: Arc::new(RwLock::new(vector_store)),
            fts_store: Arc::new(RwLock::new(fts_store)),
            writer_lock,
            readonly,
            changes_count: std::sync::atomic::AtomicU64::new(0),
            job_gate: None,
        })
    }

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

        let vector_store = ChunkStore::Local(VectorStore::new(db_path, dimensions)?);
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
        let vector_store = ChunkStore::Local(VectorStore::open_readonly(db_path, dimensions)?);
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
    /// Serve-wide embedding-service pool (`None` outside serve — CLI/standalone
    /// MCP run in their own process where direct construction is correct).
    embedding_pool: Option<Arc<EmbeddingServicePool>>,
    /// Per-language symbol indexer registry (C# etc.)
    symbol_registry: Arc<SymbolIndexerRegistry>,
}

/// Returns true if `path` has one of the TypeScript extensions tracked by the
/// file-watcher's symbol-rebuild debounce (`.ts`, `.tsx`, `.mts`, `.cts`).
/// Mirrors the inline `.cs` extension check used for the C# adapter.
fn is_ts_extension(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("ts") | Some("tsx") | Some("mts") | Some("cts")
    )
}

/// Returns true when `path` is a C# restore input (file NAME match,
/// case-insensitive): its change invalidates NuGet restore state and drives
/// a FULL C# symbol rebuild rather than the per-project incremental.
fn is_csharp_restore_input(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| {
            crate::constants::CSHARP_RESTORE_INPUT_FILES
                .iter()
                .any(|f| name.eq_ignore_ascii_case(f))
        })
}

/// Which symbol-rebuild debounce one watched file event feeds. Pure path
/// decision shared by the modify/delete/rename FSW arms so the
/// classification is table-testable without driving the async loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SymbolRebuildTrigger {
    /// Per-project incremental (.cs) rebuild.
    CSharpIncremental,
    /// FULL solution rebuild: a restore-input change invalidates restore
    /// state for every project at once (issue #225).
    CSharpRestoreInput,
    /// Full TypeScript rebuild (single tsconfig).
    TypeScript,
    /// Text-index only.
    None,
}

fn classify_symbol_rebuild_event(path: &Path) -> SymbolRebuildTrigger {
    if path.extension().and_then(|e| e.to_str()) == Some("cs") {
        SymbolRebuildTrigger::CSharpIncremental
    } else if is_csharp_restore_input(path) {
        SymbolRebuildTrigger::CSharpRestoreInput
    } else if is_ts_extension(path) {
        SymbolRebuildTrigger::TypeScript
    } else {
        SymbolRebuildTrigger::None
    }
}

/// Where an embed pass gets its `EmbeddingService`: the serve-wide pool
/// (which already holds the persistent cache's LMDB env open) or a fresh
/// service (CLI / standalone MCP, own process).
enum EmbeddingSource {
    Pool(Arc<EmbeddingServicePool>),
    Fresh,
}

/// Decide the embed source from the optional pool. Pure seam so the
/// `Some`/`None` arm choice shared by both embed call sites is unit-testable
/// without loading a model.
fn embed_service_choice(pool: Option<&Arc<EmbeddingServicePool>>) -> EmbeddingSource {
    match pool {
        Some(p) => EmbeddingSource::Pool(Arc::clone(p)),
        None => EmbeddingSource::Fresh,
    }
}

/// True when the current refresh batch should stop accumulating files before
/// reading the next one.
///
/// `consumed > 0` guarantees at least one file per batch, so a single file
/// larger than `max_chunks` still makes progress; `max_chunks == 0` disables
/// the cap entirely (the file-count batch size remains the only bound).
/// Extracted as a pure predicate so its edges are unit-testable without
/// loading ONNX.
fn chunk_cap_reached(consumed: usize, accumulated_chunks: usize, max_chunks: usize) -> bool {
    consumed > 0 && max_chunks > 0 && accumulated_chunks >= max_chunks
}

impl IndexManager {
    /// Cancellation guard shared by every cancellable indexing function.
    ///
    /// Indexing passes (`force_reindex_with_stores`,
    /// `perform_incremental_refresh_with_stores`, `refresh_index_with_stores`,
    /// `process_batch_with_stores`) receive a `CancellationToken` and call this
    /// at every safe boundary (loop tops, between phases) so a `remove_repo`
    /// mid-flight aborts promptly instead of running the full embed pass to
    /// completion on an alias that is already gone.
    ///
    /// Returns a distinct [`anyhow`] error so the caller can tell a clean
    /// cancellation apart from a genuine failure — the add-repo task checks
    /// `cancel_token.is_cancelled()` in its error branch (see
    /// `add_repo_handler`), and the FSW loop simply logs and continues.
    fn ensure_indexing_active(cancel_token: &CancellationToken) -> Result<()> {
        if cancel_token.is_cancelled() {
            return Err(anyhow::anyhow!("indexing cancelled"));
        }
        Ok(())
    }

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
            embedding_pool: None,
            symbol_registry: Arc::new(SymbolIndexerRegistry::new()),
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
    /// * `embedding_pool` - Serve-wide embedding-service pool; `Some` in serve
    ///   (reuses the process-shared persistent cache), `None` in standalone
    ///   processes (direct construction — no pool to conflict with).
    pub async fn new_without_refresh<P: AsRef<Path>>(
        codebase_path: P,
        stores: Arc<SharedStores>,
        embedding_pool: Option<Arc<EmbeddingServicePool>>,
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
            embedding_pool,
            symbol_registry: Arc::new(SymbolIndexerRegistry::new()),
        })
    }

    /// Resolve the embedding model recorded in `metadata.json` for an index.
    ///
    /// Returns the parsed [`ModelType`] and its dimension count. Vectors written
    /// to an index MUST be produced by this exact model — embedding with any
    /// other model (including the hardcoded default) silently corrupts the index
    /// and, for non-384d models, yields a dimension mismatch. Every embedding
    /// path resolves the model through this helper instead of `ModelType::default()`.
    ///
    /// Fails fast if metadata is missing, names an unknown model, or records a
    /// dimension count that disagrees with the resolved model.
    pub(crate) fn resolve_embed_model(db_path: &Path) -> Result<(ModelType, usize)> {
        let metadata_path = db_path.join("metadata.json");
        if !metadata_path.exists() {
            return Err(anyhow::anyhow!(
                "No metadata.json found in {}",
                db_path.display()
            ));
        }
        let content = std::fs::read_to_string(&metadata_path)?;
        let json: serde_json::Value = serde_json::from_str(&content)?;
        let model_name = json
            .get("model_short_name")
            .and_then(|v| v.as_str())
            .unwrap_or("minilm-l6-q");
        let dimensions = json
            .get("dimensions")
            .and_then(|v| v.as_u64())
            .unwrap_or(384) as usize;
        let model = ModelType::parse(model_name).ok_or_else(|| {
            anyhow::anyhow!(
                "Unknown embedding model '{}' in {}. Valid models: {}",
                model_name,
                metadata_path.display(),
                ModelType::valid_short_names()
            )
        })?;
        if model.dimensions() != dimensions {
            return Err(anyhow::anyhow!(
                "Metadata inconsistency in {}: model '{}' has {} dimensions but \
                 metadata records {}. The index is corrupt — re-create it with a \
                 single consistent model.",
                metadata_path.display(),
                model_name,
                model.dimensions(),
                dimensions
            ));
        }
        Ok((model, dimensions))
    }

    /// Perform incremental refresh using shared stores.
    ///
    /// This checks for changed/deleted files since last index and updates
    /// the index accordingly. Uses the shared stores to avoid lock conflicts.
    ///
    /// `embedding_pool` — when `Some`, embeddings are produced by the serve
    /// pool's service so the process-shared persistent cache is reused; a
    /// fresh `EmbeddingService` in the serve process would be refused by the
    /// process-global LMDB registry (the pool already holds the cache open)
    /// and silently degrade to cache-less re-embedding.
    ///
    /// `heartbeat` — when `Some`, fired once at start, before each slow phase,
    /// and once per batch. Callers tracking an `active_reindexes` marker pass
    /// a closure that renews it; without renewal a refresh running past
    /// `MAX_INDEXING_SECS` loses the marker and races the reaper/FSW.
    pub async fn perform_incremental_refresh_with_stores(
        codebase_path: &Path,
        db_path: &Path,
        stores: &SharedStores,
        cancel_token: &CancellationToken,
        embedding_pool: Option<&Arc<EmbeddingServicePool>>,
        heartbeat: Option<&IndexingHeartbeat>,
    ) -> Result<()> {
        // Serialise this whole refresh behind the process-wide heavy-job gate
        // (when one is attached): the permit is held across the file walk, the
        // chunk/embed batches and the final HNSW build, so at most
        // `CODESEARCH_INDEX_JOBS` repos do this at once. Queries never take it.
        // Cancellation-aware: a queued job that was cancelled aborts here and
        // drops its stores instead of parking for the holder's whole run.
        let _job_permit = match stores.acquire_job_permit_or_cancel(cancel_token).await {
            crate::limits::JobAcquire::Ready(permit) => permit,
            crate::limits::JobAcquire::Cancelled => {
                return Err(anyhow::anyhow!("indexing cancelled"));
            }
        };
        Self::perform_incremental_refresh_inner(
            codebase_path,
            db_path,
            stores,
            cancel_token,
            embedding_pool,
            heartbeat,
        )
        .await
    }

    /// Gate-free body of [`Self::perform_incremental_refresh_with_stores`].
    ///
    /// Split out for `force_reindex_with_stores`, which holds the gate permit
    /// across the destructive clear phase as well and must call this directly:
    /// re-entering the public wrapper would re-acquire the gate and deadlock
    /// under `CODESEARCH_INDEX_JOBS=1`.
    async fn perform_incremental_refresh_inner(
        codebase_path: &Path,
        db_path: &Path,
        stores: &SharedStores,
        cancel_token: &CancellationToken,
        embedding_pool: Option<&Arc<EmbeddingServicePool>>,
        heartbeat: Option<&IndexingHeartbeat>,
    ) -> Result<()> {
        use crate::cache::FileMetaStore;
        use crate::chunker::SemanticChunker;
        use crate::embed::EmbeddingService;
        use crate::file::FileWalker;

        info!("🔄 Performing incremental refresh with shared stores...");
        let start = std::time::Instant::now();

        // Bail out before reading/deriving anything if a cancellation already
        // arrived (e.g. remove_repo ran while this task was scheduled).
        Self::ensure_indexing_active(cancel_token)?;
        if let Some(hb) = heartbeat {
            hb();
        }

        // Read model name + dims (lenient) for the FileMetaStore. The strict,
        // fail-fast embedding-model resolution happens lazily below, only when
        // there are actually changed files to embed — a no-op refresh must not
        // require a resolvable model.
        let metadata_path = db_path.join("metadata.json");
        let (model_name, dimensions) = if metadata_path.exists() {
            let content = std::fs::read_to_string(&metadata_path)?;
            let json: serde_json::Value = serde_json::from_str(&content)?;
            let model = json
                .get("model_short_name")
                .and_then(|v| v.as_str())
                .unwrap_or("minilm-l6-q")
                .to_string();
            let dims = json
                .get("dimensions")
                .and_then(|v| v.as_u64())
                .unwrap_or(384) as usize;
            (model, dims)
        } else {
            return Err(anyhow::anyhow!("No metadata.json found in database"));
        };

        // Load FileMetaStore
        let mut file_meta_store = FileMetaStore::load_or_create(db_path, &model_name, dimensions)?;

        // B1 safety guard: if FileMetaStore is empty but VectorStore has chunks,
        // the metadata was lost/reset (model change, corrupt file, etc.).
        // Re-indexing without clearing would create duplicate chunks.
        if file_meta_store.is_empty() && stores.vector_store.read().await.local().is_some() {
            let vs_chunks = {
                let vs = stores.vector_store.read().await;
                vs.stats().await?.total_chunks
            };
            if vs_chunks > 0 {
                warn!(
                    "⚠️  FileMetaStore is empty but VectorStore has {} chunks — \
                     clearing stores to prevent duplicates (metadata was likely lost/reset)",
                    vs_chunks
                );
                {
                    let mut vstore = stores.vector_store.write().await;
                    vstore.clear().await?;
                }
                {
                    let mut fts = stores.fts_store.write().await;
                    fts.clear()?;
                }
            }
        }

        if super::key_migration::repair_legacy_index_shared(
            codebase_path,
            &mut file_meta_store,
            stores,
        )
        .await?
        {
            file_meta_store.save(db_path)?;
        }

        // Walk files.
        //
        // `FileWalker::walk()` is synchronous and I/O-heavy (recursive directory
        // traversal). Offload it to `spawn_blocking` so it does not block tokio
        // worker threads during warmup.
        let codebase = codebase_path.to_path_buf();
        let (files, _stats) = tokio::task::spawn_blocking(move || FileWalker::new(codebase).walk())
            .await
            .map_err(|e| anyhow::anyhow!("file walk task panicked: {}", e))??;

        // Find changed and deleted files
        let mut changed_files = Vec::new();
        let mut unchanged_count = 0;
        let remote_files = {
            let store = stores.vector_store.read().await;
            if store.local().is_none() {
                Some(store.get_chunks_by_file().await?)
            } else {
                None
            }
        };

        for file in &files {
            let key = crate::cache::storage_key(&file.path, codebase_path);
            let (needs_reindex, old_chunk_ids) = file_meta_store.check_file(&file.path, &key)?;
            let publication_incomplete = remote_files.as_ref().is_some_and(|files| {
                let mut committed = files.get(&key).cloned().unwrap_or_default();
                let mut published = old_chunk_ids.clone();
                committed.sort_unstable();
                published.sort_unstable();
                committed != published
            });
            if needs_reindex || publication_incomplete {
                changed_files.push(file.clone());
                debug!("📝 File changed: {}", file.path.display());
            } else {
                unchanged_count += 1;
            }
        }

        // Include SQL files absent from local publication metadata.
        let mut deleted_files = file_meta_store.find_deleted_files(codebase_path);
        if let Some(remote_files) = &remote_files {
            for (path, ids) in remote_files {
                if !codebase_path.join(path).exists()
                    && !deleted_files.iter().any(|(key, _)| key == path)
                {
                    deleted_files.push((path.clone(), ids.clone()));
                }
            }
        }

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

        // A cancellation that arrived during the file walk must abort BEFORE any
        // destructive store mutation below (stale-chunk deletion), so a
        // half-cleaned index is never left behind by a removed repo.
        Self::ensure_indexing_active(cancel_token)?;
        if let Some(hb) = heartbeat {
            hb();
        }

        // There is work to do. Resolve the embedding model NOW — before any
        // destructive store mutation below — so that a corrupt index (unknown
        // model / model-vs-dimension mismatch) fails fast with the index still
        // intact, rather than deleting stale chunks and then erroring before
        // re-embedding them. Fail-fast guarantees vectors are always produced
        // by the model the index was created with (never the hardcoded default).
        let (embed_model, _dims) = Self::resolve_embed_model(db_path)?;

        // Delete chunks for deleted files
        for (file_path, chunk_ids) in &deleted_files {
            if !chunk_ids.is_empty() {
                debug!("🗑️  Deleting {} chunks for: {}", chunk_ids.len(), file_path);

                // Delete from vector store
                {
                    let mut store = stores.vector_store.write().await;
                    store.delete_file(file_path).await?;
                }

                // Delete from FTS
                {
                    let mut fts_store = stores.fts_store.write().await;
                    fts_store.delete_by_path(file_path)?;
                    for chunk_id in chunk_ids {
                        fts_store.delete_chunk(*chunk_id)?;
                    }
                }
            }
            file_meta_store.remove_file(file_path);
        }

        // Delete old chunks for changed files
        for file in &changed_files {
            let key = crate::cache::storage_key(&file.path, codebase_path);
            let (_, old_chunk_ids) = file_meta_store.check_file(&file.path, &key)?;
            if !old_chunk_ids.is_empty() {
                debug!(
                    "🔄 Deleting {} old chunks for: {}",
                    old_chunk_ids.len(),
                    file.path.display()
                );

                // Delete from vector store
                {
                    let mut store = stores.vector_store.write().await;
                    if let Some(local) = store.local_mut() {
                        local.delete_chunks(&old_chunk_ids)?;
                    }
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

        // Chunk changed files — in bounded batches, not one unbounded pass.
        //
        // A single incremental refresh may need to absorb a corpus delta of
        // any size (a normal edit touches a handful of files; a vendor sync
        // can drop thousands of new files at once). Reading+chunking+embedding
        // the ENTIRE delta into one in-memory Vec before writing anything out
        // is what OOM'd a 1 vCPU/2 GiB `codesearch-serve` container when a
        // vendor `docs` corpus roughly doubled (2509 -> 5666 files) in one
        // sync. Batching bounds peak memory to O(batch), not O(total delta),
        // so a delta of any size is now safe — it just takes longer, spread
        // across sequential batches. See `INCREMENTAL_REFRESH_BATCH_SIZE`.
        //
        // A batch is additionally cut short once its accumulated chunks reach
        // `CODESEARCH_MAX_CHUNKS_PER_BATCH` (when set): file count alone is a
        // poor memory proxy when a few files can produce tens of thousands of
        // chunks.
        if !changed_files.is_empty() {
            let batch_size = std::env::var(crate::constants::INCREMENTAL_REFRESH_BATCH_SIZE_ENV)
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|&n| n > 0)
                .unwrap_or(crate::constants::INCREMENTAL_REFRESH_BATCH_SIZE);
            let max_chunks = crate::limits::max_chunks_per_batch();
            let embed_pause = crate::limits::embed_pause();
            // Upper-bound estimate for the log line; the chunk cap can split
            // this into more batches than the file count alone implies.
            let estimated_batches = changed_files.len().div_ceil(batch_size);
            info!(
                "🔄 Processing {} changed files in ~{} batch(es) of up to {} file(s) each \
                 (chunk cap per batch: {})...",
                changed_files.len(),
                estimated_batches,
                batch_size,
                if max_chunks == 0 {
                    "none".to_string()
                } else {
                    max_chunks.to_string()
                }
            );

            let cache_dir = crate::constants::get_global_models_cache_dir()?;
            let mut total_indexed = 0usize;
            let mut cursor = 0usize;
            let mut batch_idx = 0usize;

            while cursor < changed_files.len() {
                let take = (changed_files.len() - cursor).min(batch_size);
                let files_for_embed = changed_files[cursor..cursor + take].to_vec();
                // Abort between batches if the repo was removed mid-index.
                Self::ensure_indexing_active(cancel_token)?;
                // A batch of a large delta can run for minutes — renew the
                // caller's indexing marker so it survives past the stale
                // threshold while real progress is being made.
                if let Some(hb) = heartbeat {
                    hb();
                }

                // Read + chunk + embed is synchronous, CPU/I/O-heavy work
                // (file reads, tree-sitter parsing, fastembed/ONNX inference that
                // saturates all cores). Offload the whole block to `spawn_blocking`
                // so it never runs on a tokio worker thread. The `EmbeddingService`
                // and `SemanticChunker` are built inside the closure because they
                // are not needed on the async side and may not be `Send`.
                //
                // The closure consumes as many of the offered files as fit the
                // chunk cap (always at least one, so every iteration makes
                // progress) and reports how many it took, so the outer loop's
                // bookkeeping covers exactly the files that were processed.
                let cache_dir_for_batch = cache_dir.clone();
                let root_for_batch = codebase_path.to_path_buf();
                // Clone the token into the blocking closure so a cancel arriving
                // DURING the (long, core-saturating) embed pass is observed
                // per-file, not only once the whole batch returns.
                let batch_cancel = cancel_token.clone();
                let embed_source = embed_service_choice(embedding_pool);
                let (embedded_chunks, consumed) = tokio::task::spawn_blocking(
                    move || -> Result<(Vec<crate::embed::EmbeddedChunk>, usize)> {
                        let mut chunker = SemanticChunker::new(100, 2000, 10);
                        let mut all_chunks = Vec::new();
                        let mut consumed = 0usize;

                        for file in &files_for_embed {
                            // Stop once this batch has reached its chunk budget.
                            // `consumed > 0` guarantees at least one file per
                            // iteration even if that one file alone exceeds the
                            // cap, so the outer loop always advances.
                            if chunk_cap_reached(consumed, all_chunks.len(), max_chunks) {
                                break;
                            }
                            // Mid-embed cancellation point: abort inside the
                            // spawn_blocking task so we stop reading/chunking/
                            // embedding further files in this batch promptly.
                            if batch_cancel.is_cancelled() {
                                return Err(anyhow::anyhow!("indexing cancelled"));
                            }
                            // Count the file before the read: a read failure
                            // still advances the outer cursor, so the loop can
                            // never stall on an unreadable file.
                            consumed += 1;
                            let content = match std::fs::read_to_string(&file.path) {
                                Ok(c) => c,
                                Err(_) => continue,
                            };
                            // Chunk under the project-relative storage key so
                            // chunk metadata stays machine-portable.
                            let rel = std::path::PathBuf::from(crate::cache::storage_key(
                                &file.path,
                                &root_for_batch,
                            ));
                            let chunks = chunker.chunk_semantic(file.language, &rel, &content)?;
                            all_chunks.extend(chunks);
                        }

                        if all_chunks.is_empty() {
                            return Ok((Vec::new(), consumed));
                        }

                        // Serve process: the pool already holds the persistent
                        // cache's LMDB env open — a fresh `EmbeddingService`
                        // here is refused by the process-global registry and
                        // would re-embed everything without cache.
                        //
                        // `embed_chunks_yielding` holds the per-model lock for
                        // one ONNX mini-batch at a time (plus the configured
                        // pause), so an interactive query embedding is not stuck
                        // behind this whole file batch.
                        let embedded = match embed_source {
                            EmbeddingSource::Pool(pool) => {
                                pool.embed_chunks_yielding(embed_model, all_chunks, embed_pause)?
                            }
                            EmbeddingSource::Fresh => {
                                let mut embedding_service = EmbeddingService::with_cache_dir(
                                    embed_model,
                                    Some(cache_dir_for_batch.as_path()),
                                )?;
                                embedding_service.embed_chunks(all_chunks)?
                            }
                        };
                        Ok((embedded, consumed))
                    },
                )
                .await
                .map_err(|e| {
                    anyhow::anyhow!("chunk+embed task panicked (batch {}): {}", batch_idx + 1, e)
                })??;

                // The closure always consumes at least one file; guard the
                // cursor arithmetic against a future regression that would
                // otherwise spin forever.
                debug_assert!(consumed > 0, "refresh batch made no progress");
                let file_batch = &changed_files[cursor..cursor + consumed];
                cursor += consumed;
                batch_idx += 1;

                // A cancel arriving after embed completed but before we commit
                // the batch to the stores must skip the insert + the final
                // build_index, so a removed repo never receives fresh data.
                Self::ensure_indexing_active(cancel_token)?;

                if !embedded_chunks.is_empty() {
                    info!(
                        "📦 Batch {}: embedding {} chunks with model {}...",
                        batch_idx,
                        embedded_chunks.len(),
                        embed_model.short_name()
                    );

                    // Insert into vector store. `build_index()` (HNSW graph
                    // construction) is deliberately deferred until ALL batches
                    // are inserted — it rebuilds the whole graph from current
                    // storage, so doing it once at the end avoids O(batches)
                    // redundant rebuilds.
                    let chunk_ids = {
                        let mut store = stores.vector_store.write().await;
                        store
                            .insert_chunks_with_ids(embedded_chunks.clone())
                            .await?
                    };

                    // Insert into FTS
                    {
                        let mut fts_store = stores.fts_store.write().await;
                        for file in file_batch {
                            fts_store.delete_by_path(&crate::cache::storage_key(
                                &file.path,
                                codebase_path,
                            ))?;
                        }
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

                    // Update file metadata for this batch's files.
                    // Group chunks by file path (normalize for consistent lookup)
                    let mut chunks_by_file: std::collections::HashMap<String, Vec<u32>> =
                        std::collections::HashMap::new();
                    for (chunk, chunk_id) in embedded_chunks.iter().zip(chunk_ids.iter()) {
                        chunks_by_file
                            .entry(normalize_path_str(&chunk.chunk.path))
                            .or_default()
                            .push(*chunk_id);
                    }

                    for file in file_batch {
                        let path_str = crate::cache::storage_key(&file.path, codebase_path);
                        if let Some(ids) = chunks_by_file.get(&path_str) {
                            file_meta_store.update_file(&file.path, &path_str, ids.clone())?;
                        } else {
                            // File was processed but produced 0 chunks (e.g. minified JS,
                            // empty file). Track it with empty chunk list so it is not
                            // re-processed on every run and doctor doesn't flag it.
                            stores
                                .vector_store
                                .write()
                                .await
                                .delete_file(&path_str)
                                .await?;
                            file_meta_store.update_file(&file.path, &path_str, vec![])?;
                        }
                    }

                    total_indexed += embedded_chunks.len();
                } else {
                    // ALL files in this batch produced 0 chunks — still track
                    // them so they are not flagged as unindexed on every
                    // subsequent run.
                    for file in file_batch {
                        let key = crate::cache::storage_key(&file.path, codebase_path);
                        stores.vector_store.write().await.delete_file(&key).await?;
                        file_meta_store.update_file(&file.path, &key, vec![])?;
                    }
                }

                // Persist per batch: vector and FTS already commit per batch,
                // so the meta must follow or a crash/restart mid-pass leaves
                // meta empty while chunks exist — the next open then wipes
                // the whole store (the B1 guard) and re-embeds everything.
                file_meta_store.save(db_path)?;
            }

            // Build the HNSW index once, after every batch has been inserted.
            if total_indexed > 0 {
                // Don't rebuild the graph for a repo that was removed mid-index.
                Self::ensure_indexing_active(cancel_token)?;
                let vector_store = Arc::clone(&stores.vector_store);
                tokio::task::spawn_blocking(move || {
                    let mut store = vector_store.blocking_write();
                    if let Some(local) = store.local_mut() {
                        local.build_index()?;
                    }
                    Ok::<_, anyhow::Error>(())
                })
                .await
                .map_err(|e| anyhow::anyhow!("build_index task panicked: {}", e))??;
            }

            info!(
                "✅ Indexed {} chunks across {} batch(es)",
                total_indexed, batch_idx
            );
        }

        // Save file metadata
        file_meta_store.save(db_path)?;

        // Track changes for dashboard/TUI
        let total_changes = (changed_files.len() + deleted_files.len()) as u64;
        if total_changes > 0 {
            stores
                .changes_count
                .fetch_add(total_changes, std::sync::atomic::Ordering::Relaxed);
        }

        let elapsed = start.elapsed();
        info!(
            "✅ Incremental refresh completed in {:.2}s",
            elapsed.as_secs_f64()
        );

        // Persist the resolved model AND the chunk/file counts in metadata.json.
        //
        // Writing the model here (mirroring the CLI `index_with_options` path,
        // which stamps it unconditionally) unifies the two index-creation paths:
        // an index built via the serve/git-hook path — which may have started
        // from a model-less metadata.json pre-created by `ensure_schema_version`
        // — now always ends up with a resolvable `model_short_name`. This is the
        // structural half of the "model: unknown" fix: it guarantees the model
        // is recorded regardless of who created the file, so it can never regress
        // to `unknown` (which also disables the empty-index fallback). Best-effort
        // — a failed write only affects display/status, not searchability.
        {
            if let Err(e) = crate::vectordb::merge_metadata_atomic(db_path, |obj| {
                embed_model.write_metadata_fields(obj);
            }) {
                warn!("metadata.json model write warning: {}", e);
            }

            let vs = stores.vector_store.read().await;
            if let Ok(stats) = vs.stats().await {
                super::update_metadata_stats(db_path, stats.total_chunks, stats.total_files);
            }
        }

        Ok(())
    }

    /// Force a full reindex using the already-open stores.
    ///
    /// Instead of closing and deleting the LMDB database (which fails on Windows
    /// when another process holds the memory-mapped file), this:
    /// 1. Clears all data from VectorStore, FtsStore, and FileMetaStore in-place
    /// 2. Reindexes every file from scratch using the existing store handles
    ///
    /// No files are deleted, so there is no OS error 32 (file in use).
    ///
    /// A failure of the transient Windows FTS commit race (an indexer holds
    /// a freshly created Tantivy segment file while the commit opens it —
    /// os error 5/32/33) redrives the WHOLE pass, backing off 5s then 20s:
    /// a full pass is idempotent (clear + rebuild), while a commit retry on
    /// the same writer is never safe (a half-joined pipeline must not be
    /// recommitted — see `FtsStore::commit_once`). Field incidents died on
    /// a single 5s redrive while the AV still held the file; the second,
    /// longer backoff covers that window.
    pub async fn force_reindex_with_stores(
        codebase_path: &Path,
        db_path: &Path,
        stores: &SharedStores,
        model_override: Option<ModelType>,
        cancel_token: &CancellationToken,
        embedding_pool: Option<&Arc<EmbeddingServicePool>>,
    ) -> Result<()> {
        Self::retry_on_transient_fts(&Self::FTS_TRANSIENT_BACKOFFS, || async {
            Self::force_reindex_with_stores_once(
                codebase_path,
                db_path,
                stores,
                model_override,
                cancel_token,
                embedding_pool,
            )
            .await
        })
        .await
    }

    /// Redrive `attempt` on the transient Windows FTS commit race, one extra
    /// pass per entry in `backoffs`. Any other error (and a transient failure
    /// that outlives the last backoff) is returned untouched — bounded, never
    /// a loop.
    pub(crate) async fn retry_on_transient_fts<T, Fut, F>(
        backoffs: &[std::time::Duration],
        mut attempt: F,
    ) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        for backoff in backoffs {
            match attempt().await {
                Ok(v) => return Ok(v),
                Err(e)
                    if crate::fts::FtsStore::is_transient_fts_commit_error(&format!("{e:#}")) =>
                {
                    warn!(
                        "Transient FTS commit failure ({e:#}) — redriving the full pass in {}s",
                        backoff.as_secs().max(1)
                    );
                    tokio::time::sleep(*backoff).await;
                }
                Err(e) => return Err(e),
            }
        }
        attempt().await
    }

    /// Backoffs for the transient Windows FTS commit race redrive: one extra
    /// full pass per entry, then a final attempt. Shared by force reindex and
    /// the per-file incremental paths so they cannot drift apart.
    const FTS_TRANSIENT_BACKOFFS: [std::time::Duration; 2] = [
        std::time::Duration::from_secs(5),
        std::time::Duration::from_secs(20),
    ];

    async fn force_reindex_with_stores_once(
        codebase_path: &Path,
        db_path: &Path,
        stores: &SharedStores,
        model_override: Option<ModelType>,
        cancel_token: &CancellationToken,
        embedding_pool: Option<&Arc<EmbeddingServicePool>>,
    ) -> Result<()> {
        use crate::cache::FileMetaStore;
        use anyhow::Context;

        info!("🔄 Force reindex: clearing all store data in-place...");

        // Bail before clearing any store data if the repo was already removed.
        Self::ensure_indexing_active(cancel_token)?;

        // Hold the process-wide heavy-job gate across the destructive clear AND
        // the full reindex. The nested call goes to the gate-free inner body so
        // the permit is not re-acquired (which would deadlock under
        // `CODESEARCH_INDEX_JOBS=1`). The wait is cancellation-aware so a repo
        // removed while queued aborts before the destructive clear.
        let _job_permit = match stores.acquire_job_permit_or_cancel(cancel_token).await {
            crate::limits::JobAcquire::Ready(permit) => permit,
            crate::limits::JobAcquire::Cancelled => {
                return Err(anyhow::anyhow!("indexing cancelled"));
            }
        };
        // Re-check after the (possibly long) wait: a cancellation that landed
        // while queued must not start the clear, and the stores are still
        // intact at this point.
        Self::ensure_indexing_active(cancel_token)?;

        // ── Step 0: Read and preserve metadata BEFORE clearing anything ──
        // This is defensive: the DB may be incomplete (no metadata.json at all),
        // or clear() may indirectly remove it. We preserve model info so we can
        // write it back before the reindex starts.
        let metadata_path = db_path.join("metadata.json");
        let mut preserved_metadata: serde_json::Value = if metadata_path.exists() {
            let content = std::fs::read_to_string(&metadata_path)
                .with_context(|| format!("Failed to read {}", metadata_path.display()))?;
            serde_json::from_str(&content)
                .with_context(|| format!("Failed to parse {}", metadata_path.display()))?
        } else {
            warn!(
                "⚠️  No metadata.json found in {} — using defaults for force reindex",
                db_path.display()
            );
            serde_json::json!({
                "model_short_name": "minilm-l6-q",
                "model_name": "all-MiniLM-L6-v2-q",
                "dimensions": 384,
            })
        };

        // Apply model override if provided (e.g. from `index add --model`)
        if let Some(ref mt) = model_override {
            if let Some(obj) = preserved_metadata.as_object_mut() {
                mt.write_metadata_fields(obj);
            }
            info!(
                "📝 Model override applied: {} ({} dims)",
                mt.short_name(),
                mt.dimensions()
            );
        }

        // If the metadata.json that already exists lacks model fields, stamp the
        // default model. This is the fix for the "model: unknown" worktree bug:
        // when a repo is registered via `POST /repos` (the git-hook path), the
        // store is opened first and `ensure_schema_version` pre-creates a
        // metadata.json containing only `schema_version`. That defeats the
        // `else` branch above (which only stamps a default when the whole file is
        // absent), so without this guard the index is left with no
        // `model_short_name` — every reader then shows `model: unknown` AND the
        // live-chunk-count fallback (`live_chunk_count`) bails on that string,
        // making a perfectly-good index look empty. Only runs when no explicit
        // override was given (the override block above already populated these).
        if preserved_metadata.get("model_short_name").is_none() {
            let default_model = ModelType::default();
            if let Some(obj) = preserved_metadata.as_object_mut() {
                default_model.write_metadata_fields(obj);
            }
            info!(
                "📝 metadata.json had no model_short_name (pre-created by schema-version bootstrap) — stamping default model {} ({} dims)",
                default_model.short_name(),
                default_model.dimensions()
            );
        }
        let model_name = preserved_metadata
            .get("model_short_name")
            .and_then(|v| v.as_str())
            .unwrap_or("minilm-l6-q")
            .to_string();
        let dimensions = preserved_metadata
            .get("dimensions")
            .and_then(|v| v.as_u64())
            .unwrap_or(384) as usize;

        // ── Step 1: Clear vector store (LMDB databases) ──
        {
            let mut vstore = stores.vector_store.write().await;
            vstore.validate_embedding_space(&model_name, dimensions)?;
            vstore.clear().await?;
        }

        // ── Step 2: Clear full-text search store (Tantivy index) ──
        {
            let mut fts = stores.fts_store.write().await;
            fts.clear()?;
        }

        // ── Step 3: Clear file metadata so every file is treated as new ──
        {
            let mut file_meta = FileMetaStore::load_or_create(db_path, &model_name, dimensions)?;
            file_meta.clear();
            file_meta.save(db_path)?;
        }

        // ── Step 4: Ensure metadata.json exists before reindex ──
        // perform_incremental_refresh_with_stores() hard-fails without it.
        // Write back the preserved metadata (with updated timestamp) via the
        // atomic read-modify-write helper so a crash here can never leave a
        // truncated/partial metadata.json (the failure mode the atomic writer
        // exists to prevent).
        {
            let indexed_at = serde_json::Value::String(chrono::Utc::now().to_rfc3339());
            let preserved = preserved_metadata.clone();
            crate::vectordb::merge_metadata_atomic(db_path, move |obj| {
                if let Some(src) = preserved.as_object() {
                    for (k, v) in src {
                        obj.insert(k.clone(), v.clone());
                    }
                }
                obj.insert("indexed_at".to_string(), indexed_at);
            })
            .with_context(|| format!("Failed to write {}", metadata_path.display()))?;
        }

        info!("✅ Stores cleared, metadata preserved. Starting full reindex...");

        // ── Step 5: Reindex — all files treated as "changed" since metadata is empty ──
        Self::perform_incremental_refresh_inner(
            codebase_path,
            db_path,
            stores,
            cancel_token,
            embedding_pool,
            None,
        )
        .await
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

    /// Trigger a fire-and-forget FULL symbol rebuild for every applicable
    /// language, used when a branch switch invalidates the symbol index wholesale.
    ///
    /// A branch change rewrites arbitrary files in the working tree, so an
    /// incremental (per-file / per-`.csproj`) scope cannot be computed — the
    /// buffered `.cs`/`.ts` events were discarded by the branch-change handler.
    /// A `RebuildScope::Full` is the honest, correct choice here: it re-derives
    /// the entire symbol index for the new branch. Runs in a detached blocking
    /// task so the watcher loop is never blocked by the (potentially 35–84s)
    /// scip-csharp / scip-typescript invocation.
    ///
    /// `indexing_cb` (if any) toggles the general TUI "Indexing" label around
    /// the whole rebuild; `csharp_notifier` (if any) drives the C#-specific
    /// indicator (`Started`/`Succeeded`/`Failed`). Non-applicable languages
    /// (no `.sln` / no `tsconfig.json`) or an unavailable helper are skipped
    /// without touching any status — mirroring the debounce path.
    ///
    /// Serializes with the other two C# rebuild tasks via the shared `rebuild_guard`.
    #[allow(clippy::too_many_arguments)]
    fn spawn_branch_change_symbol_rebuild(
        symbol_registry: Arc<SymbolIndexerRegistry>,
        repo_path: PathBuf,
        db_path: PathBuf,
        repo_label: String,
        csharp_notifier: Option<CSharpRebuildNotifier>,
        indexing_cb: Option<IndexingStatusCallback>,
        cancel_token: CancellationToken,
        rebuild_guard: Arc<std::sync::Mutex<()>>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::task::spawn_blocking(move || {
            // Two rebuilds on one LMDB db must not overlap.
            let _rebuild_guard = rebuild_guard.lock().unwrap_or_else(|p| p.into_inner());

            // Resolve applicable + available indexers up front so we only toggle
            // the "Indexing" label when there is real work to do.
            let csharp = symbol_registry
                .get(LANG_CSHARP)
                .filter(|i| i.applies_to(&repo_path) && i.is_available());
            let typescript = symbol_registry
                .get(LANG_TYPESCRIPT)
                .filter(|i| i.applies_to(&repo_path) && i.is_available());

            if csharp.is_none() && typescript.is_none() {
                // Nothing to rebuild — don't flash the TUI or touch status.
                return;
            }

            if let Some(ref cb) = indexing_cb {
                cb(true);
            }

            // Check-before-start bounds each language's rebuild: a single
            // `indexer.rebuild()` call can't be interrupted mid-run (the 35–84s
            // scip-csharp invocation), but we skip languages whose rebuild hadn't
            // begun yet once cancellation lands.
            if let Some(indexer) = csharp {
                if cancel_token.is_cancelled() {
                    info!(
                        "🛑 [{}] symbol rebuild cancelled before C# rebuild",
                        repo_label
                    );
                } else {
                    // C# drives the serve-side status indicator: Started now,
                    // terminal signal inside run_full_rebuild_logged.
                    if let Some(ref n) = csharp_notifier {
                        n(SymbolRebuildSignal::Started);
                    }
                    Self::run_full_rebuild_logged(
                        indexer,
                        &repo_path,
                        &db_path,
                        &repo_label,
                        "C#",
                        csharp_notifier.as_ref(),
                    );
                }
            }

            if let Some(indexer) = typescript {
                if cancel_token.is_cancelled() {
                    info!(
                        "🛑 [{}] symbol rebuild cancelled before TypeScript rebuild",
                        repo_label
                    );
                } else {
                    // The TypeScript path has no serve-side status notifier yet, so
                    // only the general "Indexing" label reflects it (via indexing_cb).
                    Self::run_full_rebuild_logged(
                        indexer,
                        &repo_path,
                        &db_path,
                        &repo_label,
                        "TypeScript",
                        None,
                    );
                }
            }

            if let Some(ref cb) = indexing_cb {
                cb(false);
            }
        })
    }

    /// Run a `RebuildScope::Full` rebuild for one language's indexer, log the
    /// outcome with the repo + language label, and (when `notifier` is `Some`,
    /// i.e. C#) emit the terminal [`SymbolRebuildSignal`] (`Succeeded`/`Failed`).
    ///
    /// This is the shared body behind every full-scope rebuild in the watcher
    /// (branch-change C#/TS and the `.cs` debounce full-solution fallback), so
    /// the log wording and notifier semantics stay in one place. The caller
    /// owns the *in-progress* signalling (`indexing_cb(true/false)` and the C#
    /// `Started` signal), because a single caller may batch several rebuilds
    /// under one "Indexing" window.
    fn run_full_rebuild_logged(
        indexer: &dyn SymbolIndexer,
        repo_path: &Path,
        db_path: &Path,
        repo_label: &str,
        lang_label: &str,
        notifier: Option<&CSharpRebuildNotifier>,
    ) {
        match indexer.rebuild(repo_path, db_path, RebuildScope::Full) {
            Ok(summary) => {
                info!(
                    "✅ [{}] {} symbol rebuild complete: {} symbols, {} refs in {}ms",
                    repo_label,
                    lang_label,
                    summary.symbols_indexed,
                    summary.references_stored,
                    summary.duration_ms
                );
                if let Some(n) = notifier {
                    n(SymbolRebuildSignal::Succeeded {
                        index_warnings: summary.index_warnings.clone(),
                    });
                }
            }
            Err(e) if crate::symbols::csharp::is_restore_exhausted(&e) => {
                // Restore never finished within the retry cap: the previous
                // index stays live, so this is a warning, never a red error.
                warn!(
                    "⚠️ [{}] {} symbol rebuild abandoned — dotnet restore did not finish, \
                     previous index stays live: {:#}",
                    repo_label, lang_label, e
                );
                if let Some(n) = notifier {
                    n(SymbolRebuildSignal::Abandoned(format!("{e:#}")));
                }
            }
            Err(e) => {
                // `{:#}` — the whole chain. Plain `{}` prints only the outermost
                // context, which hides the LMDB error under its put context.
                warn!(
                    "⚠️ [{}] {} symbol rebuild failed: {:#}",
                    repo_label, lang_label, e
                );
                if let Some(n) = notifier {
                    n(SymbolRebuildSignal::Failed(format!("{e:#}")));
                }
            }
        }
    }

    /// Start the background file watcher.
    ///
    /// This is the **second method call** - should be called after `new()`.
    /// Spawns a background task that watches for file changes and refreshes the index.
    ///
    /// # Arguments
    /// * `cancel_token` — Cancellation token for graceful shutdown.
    /// * `csharp_notifier` — Optional callback invoked after each watcher-triggered C# symbol
    ///   rebuild. Pass `Some(notifier)` from the serve layer to propagate rebuild outcomes to the
    ///   TUI (`csharp_index_status` / `csharp_index_error`). `None` is valid for standalone /
    ///   test use where no TUI status tracking is needed.
    /// * `indexing_status_cb` — Optional callback invoked with `true` when text/vector indexing
    ///   starts and `false` when it completes. The serve layer uses this to update
    ///   `active_reindexes` so the TUI shows "Indexing" during watcher-triggered refreshes.
    ///   `None` is valid for standalone/test use.
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
    pub async fn start_file_watcher(
        &self,
        cancel_token: CancellationToken,
        csharp_notifier: Option<CSharpRebuildNotifier>,
        indexing_status_cb: Option<IndexingStatusCallback>,
    ) -> Result<()> {
        let path = self.codebase_path.clone();
        let db_path = self.db_path.clone();
        let watcher = self.watcher.clone();
        let stores = self.stores.clone();
        let git_head_watcher = self.git_head_watcher.clone();
        let symbol_registry = self.symbol_registry.clone();
        let embedding_pool = self.embedding_pool.clone();
        let indexing_cb = indexing_status_cb.clone();

        info!("🚀 Starting background file watcher...");

        // Spawn background task
        tokio::spawn(async move {
            // Short human-readable repo label for log attribution in a
            // multi-repo hub. In serve mode the alias == directory name, so the
            // last path component is the alias for the common case; fall back to
            // the full path when there is no file name (e.g. a root path).
            let repo_label = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            info!(
                "👀 File watcher task started for '{}': {}",
                repo_label,
                path.display()
            );

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

            // Symbol indexer debounce: .cs files are buffered separately and
            // flushed after SCIP_CSHARP_DEBOUNCE_MS of quiet time.
            // `cs_files_modified` — files that were added or changed (included in changed).
            // `cs_files_deleted` — files that were deleted; must be passed explicitly to the
            // incremental rebuild so their LMDB entries are purged even though they're absent
            // from the new scip-csharp output.
            let mut cs_files_modified: HashSet<PathBuf> = HashSet::new();
            let mut cs_files_deleted: HashSet<PathBuf> = HashSet::new();
            let mut cs_last_event_time: Option<std::time::Instant> = None;
            let cs_debounce = std::time::Duration::from_millis(SCIP_CSHARP_DEBOUNCE_MS);
            // C# restore inputs (Directory.Packages.props, NuGet.config, ...)
            // use the same debounce length but their OWN timer: a CPM bump
            // invalidates the whole solution's restore state, so the flush is
            // a FULL rebuild, not the per-project incremental (issue #225).
            let mut restore_inputs_changed = false;
            let mut restore_input_last_event_time: Option<std::time::Instant> = None;
            // Serializes the detached symbol rebuild tasks (incremental, restore-input full, branch-change full).
            let symbol_rebuild_guard = Arc::new(std::sync::Mutex::new(()));

            // Symbol indexer debounce: .ts/.tsx/.mts/.cts files are buffered separately
            // and flushed after SCIP_TYPESCRIPT_DEBOUNCE_MS of quiet time. Unlike C#'s
            // per-.csproj grouping, the TypeScript MVP only supports a single root
            // tsconfig.json (no monorepo multi-project resolution), so any tracked
            // change simply triggers one full rebuild — there is no per-file grouping
            // to compute, and `ts_files_modified`/`ts_files_deleted` only exist to
            // decide *whether* to flush and to log counts.
            let mut ts_files_modified: HashSet<PathBuf> = HashSet::new();
            let mut ts_files_deleted: HashSet<PathBuf> = HashSet::new();
            let mut ts_last_event_time: Option<std::time::Instant> = None;
            let ts_debounce = std::time::Duration::from_millis(SCIP_TYPESCRIPT_DEBOUNCE_MS);

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
                            info!(
                                "🔀 [{}] Git branch changed, triggering full incremental refresh...",
                                repo_label
                            );
                            // Notify serve layer: indexing active
                            if let Some(ref cb) = indexing_cb {
                                cb(true);
                            }
                            // Perform a real incremental refresh: walk filesystem,
                            // detect changed/deleted files, clean stale chunks, re-index
                            if let Err(e) = Self::refresh_index_with_stores(
                                &path,
                                &db_path,
                                &stores,
                                &cancel_token,
                                embedding_pool.as_ref(),
                            )
                            .await
                            {
                                error!("❌ [{}] Branch change refresh failed: {}", repo_label, e);
                            }
                            // Notify serve layer: indexing idle
                            if let Some(ref cb) = indexing_cb {
                                cb(false);
                            }
                            // Clear any buffered file events that arrived during the
                            // branch switch — the full refresh already handled everything
                            files_to_index.clear();
                            files_to_remove.clear();
                            cs_files_modified.clear();
                            cs_files_deleted.clear();
                            cs_last_event_time = None;
                            ts_files_modified.clear();
                            ts_files_deleted.clear();
                            ts_last_event_time = None;

                            // A branch switch can change arbitrary source files, so
                            // the symbol index is now stale — but no incremental
                            // scope can be computed (the working tree changed wholesale
                            // and the buffered .cs/.ts events were just discarded above).
                            // Trigger a fire-and-forget FULL symbol rebuild for every
                            // language that applies, so `find_impact` reflects the new
                            // branch instead of silently serving stale references.
                            drop(Self::spawn_branch_change_symbol_rebuild(
                                symbol_registry.clone(),
                                path.clone(),
                                db_path.clone(),
                                repo_label.clone(),
                                csharp_notifier.clone(),
                                indexing_cb.clone(),
                                cancel_token.clone(),
                                symbol_rebuild_guard.clone(),
                            ));
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
                                files_to_index.insert(p.clone());
                                // Track modifications for the symbol rebuild debounces
                                match classify_symbol_rebuild_event(&p) {
                                    SymbolRebuildTrigger::CSharpIncremental => {
                                        // If previously queued as deleted, promote to modified
                                        cs_files_deleted.remove(&p);
                                        cs_files_modified.insert(p);
                                        cs_last_event_time = Some(now);
                                    }
                                    SymbolRebuildTrigger::CSharpRestoreInput => {
                                        restore_inputs_changed = true;
                                        restore_input_last_event_time = Some(now);
                                    }
                                    SymbolRebuildTrigger::TypeScript => {
                                        ts_files_deleted.remove(&p);
                                        ts_files_modified.insert(p);
                                        ts_last_event_time = Some(now);
                                    }
                                    SymbolRebuildTrigger::None => {}
                                }
                            }
                            FileEvent::Deleted(p) => {
                                // If file was marked for indexing, cancel that
                                files_to_index.remove(&p);
                                files_to_remove.insert(p.clone());
                                // Track deletions per debounce — .cs deletions must
                                // be passed explicitly to the incremental rebuild so
                                // their LMDB entries are purged even though they're
                                // absent from the new scip-csharp output.
                                match classify_symbol_rebuild_event(&p) {
                                    SymbolRebuildTrigger::CSharpIncremental => {
                                        cs_files_modified.remove(&p);
                                        cs_files_deleted.insert(p);
                                        cs_last_event_time = Some(now);
                                    }
                                    SymbolRebuildTrigger::CSharpRestoreInput => {
                                        // A deleted restore input changes restore
                                        // semantics just as much as an edited one.
                                        restore_inputs_changed = true;
                                        restore_input_last_event_time = Some(now);
                                    }
                                    SymbolRebuildTrigger::TypeScript => {
                                        ts_files_modified.remove(&p);
                                        ts_files_deleted.insert(p);
                                        ts_last_event_time = Some(now);
                                    }
                                    SymbolRebuildTrigger::None => {}
                                }
                            }
                            FileEvent::Renamed(old_p, new_p) => {
                                // Remove old path, index new path
                                files_to_index.remove(&old_p);
                                files_to_remove.insert(old_p.clone());
                                files_to_remove.remove(&new_p);
                                files_to_index.insert(new_p.clone());
                                // A rename is a deletion of the old path and a
                                // modification of the new one; each side feeds its
                                // own debounce independently (a .cs → restore-input
                                // rename must trip both).
                                match classify_symbol_rebuild_event(&old_p) {
                                    SymbolRebuildTrigger::CSharpIncremental => {
                                        cs_files_modified.remove(&old_p);
                                        cs_files_deleted.insert(old_p);
                                        cs_last_event_time = Some(now);
                                    }
                                    SymbolRebuildTrigger::CSharpRestoreInput => {
                                        restore_inputs_changed = true;
                                        restore_input_last_event_time = Some(now);
                                    }
                                    SymbolRebuildTrigger::TypeScript => {
                                        ts_files_modified.remove(&old_p);
                                        ts_files_deleted.insert(old_p);
                                        ts_last_event_time = Some(now);
                                    }
                                    SymbolRebuildTrigger::None => {}
                                }
                                match classify_symbol_rebuild_event(&new_p) {
                                    SymbolRebuildTrigger::CSharpIncremental => {
                                        cs_files_deleted.remove(&new_p);
                                        cs_files_modified.insert(new_p);
                                        cs_last_event_time = Some(now);
                                    }
                                    SymbolRebuildTrigger::CSharpRestoreInput => {
                                        restore_inputs_changed = true;
                                        restore_input_last_event_time = Some(now);
                                    }
                                    SymbolRebuildTrigger::TypeScript => {
                                        ts_files_deleted.remove(&new_p);
                                        ts_files_modified.insert(new_p);
                                        ts_last_event_time = Some(now);
                                    }
                                    SymbolRebuildTrigger::None => {}
                                }
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
                        "📦 [{}] Flushing batch: {} to index, {} to remove",
                        repo_label,
                        to_index.len(),
                        to_remove.len()
                    );

                    // Signal "Indexing" to the TUI for the duration of the text
                    // batch refresh. Without this, ordinary file edits (the most
                    // common watcher activity) never surface in the TUI status
                    // column — only branch changes and symbol rebuilds did.
                    if let Some(ref cb) = indexing_cb {
                        cb(true);
                    }
                    // Process batch using shared stores
                    if let Err(e) = Self::process_batch_with_stores(
                        &path,
                        &db_path,
                        &stores,
                        to_index,
                        to_remove,
                        &cancel_token,
                        embedding_pool.as_ref(),
                    )
                    .await
                    {
                        error!("❌ [{}] Batch processing failed: {}", repo_label, e);
                    }
                    // Clear "Indexing" regardless of outcome.
                    if let Some(ref cb) = indexing_cb {
                        cb(false);
                    }

                    // Reset timer
                    last_event_time = now;
                }

                // Check if we should flush the .cs symbol rebuild debounce
                let has_cs_changes = !cs_files_modified.is_empty() || !cs_files_deleted.is_empty();
                if has_cs_changes {
                    if let Some(cs_last) = cs_last_event_time {
                        let elapsed = now.duration_since(cs_last);
                        if elapsed >= cs_debounce {
                            let modified_count = cs_files_modified.len();
                            let deleted_count = cs_files_deleted.len();
                            let cs_modified: Vec<PathBuf> = cs_files_modified.drain().collect();
                            let cs_deleted: Vec<PathBuf> = cs_files_deleted.drain().collect();
                            cs_last_event_time = None;

                            info!(
                                "🔬 [{}] {} modified + {} deleted .cs file(s), triggering incremental symbol rebuild (after {}s debounce)",
                                repo_label, modified_count, deleted_count,
                                cs_debounce.as_secs()
                            );

                            // Group changed (modified) files by .csproj so we can index per project.
                            // Each group triggers a separate incremental rebuild with
                            // --filter-project, which is much faster than rebuilding
                            // the entire solution.
                            //
                            // Deleted files are passed to every group as `deleted` so that their
                            // stale LMDB entries are purged regardless of which project they
                            // belonged to (we can't discover their csproj — they're gone).
                            let reg = symbol_registry.clone();
                            let rp = path.clone();
                            let dp = db_path.clone();
                            // Clone the repo label into the blocking task (the outer
                            // binding is reused by later loop iterations).
                            let repo_label = repo_label.clone();
                            let notifier = csharp_notifier.clone();
                            // Clone indexing_cb so the SCIP rebuild can signal
                            // active_reindexes (and therefore show "Indexing" in
                            // the TUI) for the duration of the symbol rebuild —
                            // separate from the text-index callback used for
                            // branch-change refreshes above.
                            let indexing_cb_scip = indexing_cb.clone();
                            let guard = symbol_rebuild_guard.clone();
                            tokio::task::spawn_blocking(move || {
                                // Two rebuilds on one LMDB db must not overlap.
                                let _rebuild_guard =
                                    guard.lock().unwrap_or_else(|p| p.into_inner());
                                if let Some(indexer) = reg.get(LANG_CSHARP) {
                                    if !indexer.applies_to(&rp) {
                                        info!(
                                            "🔬 [{}] symbol rebuild skipped: not applicable (no .sln)",
                                            repo_label
                                        );
                                        return;
                                    }
                                    if !indexer.is_available() {
                                        info!(
                                            "🔬 [{}] symbol rebuild skipped: helper not available",
                                            repo_label
                                        );
                                        return;
                                    }

                                    // Signal "Indexing" to the TUI now that we know
                                    // a real SCIP rebuild will actually run. This
                                    // toggles both the general repo-state label
                                    // (indexing_cb → active_reindexes) and the
                                    // C#-specific indicator (notifier → Indexing).
                                    if let Some(ref cb) = indexing_cb_scip {
                                        cb(true);
                                    }
                                    if let Some(ref n) = notifier {
                                        n(SymbolRebuildSignal::Started);
                                    }

                                    // Group modified files by their containing .csproj
                                    let mut groups: std::collections::HashMap<
                                        PathBuf,
                                        Vec<PathBuf>,
                                    > = std::collections::HashMap::new();
                                    let mut ungrouped: Vec<PathBuf> = Vec::new();

                                    for file in &cs_modified {
                                        if let Some(csproj) =
                                            crate::symbols::csharp::CSharpSymbolIndexer::find_csproj_for_file(&rp, file)
                                        {
                                            groups.entry(csproj).or_default().push(file.clone());
                                        } else {
                                            ungrouped.push(file.clone());
                                        }
                                    }

                                    // If any modified files couldn't be mapped to a .csproj,
                                    // fall back to a full solution rebuild (previously this
                                    // incorrectly used RebuildScope::Files which only used
                                    // files.first() and silently ignored the rest).
                                    if !ungrouped.is_empty() {
                                        info!(
                                            "🔬 [{}] {} modified file(s) could not be mapped to a .csproj, falling back to full solution rebuild",
                                            repo_label,
                                            ungrouped.len()
                                        );
                                        Self::run_full_rebuild_logged(
                                            indexer,
                                            &rp,
                                            &dp,
                                            &repo_label,
                                            "C#",
                                            notifier.as_ref(),
                                        );
                                        // Clear "Indexing" regardless of outcome
                                        if let Some(ref cb) = indexing_cb_scip {
                                            cb(false);
                                        }
                                        return;
                                    }

                                    // Rebuild each project group separately.
                                    // Deleted files are forwarded to every group so they're
                                    // cleaned up from LMDB regardless of origin project.
                                    let total_groups = groups.len();
                                    let mut last_error: Option<String> = None;
                                    // Restore-exhausted groups land here instead:
                                    // LMDB untouched, previous index stays live.
                                    let mut last_abandon: Option<String> = None;
                                    // Index-level warnings survived by any
                                    // group's rebuild — deduped across groups
                                    // (MSBuild repeats per project).
                                    let mut index_warnings: Vec<String> = Vec::new();
                                    let mut seen_warnings = std::collections::HashSet::new();
                                    for (i, (csproj, files)) in groups.into_iter().enumerate() {
                                        let csproj_name = csproj
                                            .file_name()
                                            .map(|n| n.to_string_lossy().into_owned())
                                            .unwrap_or_default();
                                        info!(
                                            "🔬 [{}] incremental rebuild [{}/{}]: {} ({} modified, {} deleted)",
                                            repo_label,
                                            i + 1,
                                            total_groups,
                                            csproj_name,
                                            files.len(),
                                            cs_deleted.len(),
                                        );
                                        let scope = RebuildScope::Files {
                                            changed: files,
                                            deleted: cs_deleted.clone(),
                                        };
                                        match indexer.rebuild(&rp, &dp, scope) {
                                            Ok(summary) => {
                                                for w in &summary.index_warnings {
                                                    if seen_warnings.insert(w.clone()) {
                                                        index_warnings.push(w.clone());
                                                    }
                                                }
                                                if summary.symbols_indexed == 0 {
                                                    warn!(
                                                        "⚠️ [{}/{}] Symbol rebuild returned 0 symbols for '{}' — \
                                                         project may have failed to load. Check scip-csharp logs above.",
                                                        i + 1,
                                                        total_groups,
                                                        csproj_name
                                                    );
                                                } else {
                                                    info!(
                                                        "✅ [{}/{}] Symbol rebuild complete ({}): {} symbols, {} refs in {}ms",
                                                        i + 1,
                                                        total_groups,
                                                        csproj_name,
                                                        summary.symbols_indexed,
                                                        summary.references_stored,
                                                        summary.duration_ms
                                                    );
                                                }
                                            }
                                            Err(e) => {
                                                // `{:#}` for the same reason as the
                                                // full-rebuild path: the put context
                                                // would otherwise hide the MDB_* code.
                                                // A restore-exhausted group is an
                                                // abandonment (previous index stays
                                                // live), not a failure.
                                                if crate::symbols::csharp::is_restore_exhausted(&e)
                                                {
                                                    warn!(
                                                        "⚠️ [{}/{}] Symbol rebuild abandoned ({}): dotnet \
                                                         restore did not finish — previous index stays live: {:#}",
                                                        i + 1,
                                                        total_groups,
                                                        csproj_name,
                                                        e
                                                    );
                                                    if last_abandon.is_none() {
                                                        last_abandon = Some(format!("{e:#}"));
                                                    }
                                                } else {
                                                    warn!(
                                                        "⚠️ [{}/{}] Symbol rebuild failed ({}): {:#}",
                                                        i + 1,
                                                        total_groups,
                                                        csproj_name,
                                                        e
                                                    );
                                                    last_error = Some(format!("{e:#}"));
                                                }
                                            }
                                        }
                                    }
                                    // Notify serve layer about overall outcome.
                                    // A real failure wins over an abandonment:
                                    // exhausted groups leave the previous index
                                    // live (warning), but a genuine failure must
                                    // still render red.
                                    if let Some(ref n) = notifier {
                                        n(grouped_rebuild_signal(
                                            last_error,
                                            last_abandon,
                                            index_warnings,
                                        ));
                                    }
                                    // Clear "Indexing" now that all groups are done
                                    if let Some(ref cb) = indexing_cb_scip {
                                        cb(false);
                                    }
                                }
                            });
                        }
                    }
                }

                // Flush the restore-input debounce: a changed/deleted
                // Directory.Packages.props (or sibling) invalidates restore
                // state for EVERY project at once, so the flush is a FULL
                // solution rebuild — a per-project incremental would index
                // mid-restore against stale assets (issue #225). The helper's
                // own gate refuses to index while the triggered restore is
                // still running; rebuild()'s retry loop waits it out.
                if restore_inputs_changed {
                    if let Some(restore_last) = restore_input_last_event_time {
                        let elapsed = now.duration_since(restore_last);
                        if elapsed >= cs_debounce {
                            restore_inputs_changed = false;
                            restore_input_last_event_time = None;

                            info!(
                                "🔬 [{}] C# restore input changed, triggering full symbol rebuild (after {}s debounce)",
                                repo_label,
                                cs_debounce.as_secs()
                            );

                            let reg = symbol_registry.clone();
                            let rp = path.clone();
                            let dp = db_path.clone();
                            // Clone the repo label into the blocking task (the outer
                            // binding is reused by later loop iterations).
                            let repo_label = repo_label.clone();
                            let notifier = csharp_notifier.clone();
                            let indexing_cb_scip = indexing_cb.clone();
                            let guard = symbol_rebuild_guard.clone();
                            tokio::task::spawn_blocking(move || {
                                // Two rebuilds on one LMDB db must not overlap.
                                let _rebuild_guard =
                                    guard.lock().unwrap_or_else(|p| p.into_inner());
                                if let Some(indexer) = reg.get(LANG_CSHARP) {
                                    if !indexer.applies_to(&rp) {
                                        info!(
                                            "🔬 [{}] symbol rebuild skipped: not applicable (no .sln)",
                                            repo_label
                                        );
                                        return;
                                    }
                                    if !indexer.is_available() {
                                        info!(
                                            "🔬 [{}] symbol rebuild skipped: helper not available",
                                            repo_label
                                        );
                                        return;
                                    }
                                    if let Some(ref cb) = indexing_cb_scip {
                                        cb(true);
                                    }
                                    if let Some(ref n) = notifier {
                                        n(SymbolRebuildSignal::Started);
                                    }
                                    Self::run_full_rebuild_logged(
                                        indexer,
                                        &rp,
                                        &dp,
                                        &repo_label,
                                        "C#",
                                        notifier.as_ref(),
                                    );
                                    // Clear "Indexing" regardless of outcome
                                    if let Some(ref cb) = indexing_cb_scip {
                                        cb(false);
                                    }
                                }
                            });
                        }
                    }
                }

                // Check if we should flush the .ts/.tsx/.mts/.cts symbol rebuild debounce.
                // Unlike the C# path there is no per-.csproj grouping: TypeScript MVP
                // only supports a single root tsconfig.json, so any tracked change
                // simply triggers one full rebuild via the registry's TypeScript
                // indexer (RebuildScope::Files would fall back to Full internally
                // anyway — passing Full directly here is more honest about what
                // actually happens).
                let has_ts_changes = !ts_files_modified.is_empty() || !ts_files_deleted.is_empty();
                if has_ts_changes {
                    if let Some(ts_last) = ts_last_event_time {
                        let elapsed = now.duration_since(ts_last);
                        if elapsed >= ts_debounce {
                            let modified_count = ts_files_modified.len();
                            let deleted_count = ts_files_deleted.len();
                            ts_files_modified.clear();
                            ts_files_deleted.clear();
                            ts_last_event_time = None;

                            info!(
                                "🔬 [{}] {} modified + {} deleted .ts/.tsx/.mts/.cts file(s), triggering full symbol rebuild (after {}s debounce)",
                                repo_label, modified_count, deleted_count,
                                ts_debounce.as_secs()
                            );

                            let reg = symbol_registry.clone();
                            let rp = path.clone();
                            let dp = db_path.clone();
                            let indexing_cb_ts = indexing_cb.clone();
                            // Clone the repo label into the blocking task (the outer
                            // binding is reused by later loop iterations).
                            let repo_label = repo_label.clone();
                            tokio::task::spawn_blocking(move || {
                                if let Some(indexer) = reg.get(LANG_TYPESCRIPT) {
                                    if !indexer.applies_to(&rp) {
                                        info!(
                                            "🔬 [{}] TypeScript symbol rebuild skipped: not applicable (no tsconfig.json)",
                                            repo_label
                                        );
                                        return;
                                    }
                                    if !indexer.is_available() {
                                        info!(
                                            "🔬 [{}] TypeScript symbol rebuild skipped: scip-typescript not available",
                                            repo_label
                                        );
                                        return;
                                    }

                                    // Signal "Indexing" to the TUI now that we know
                                    // a real SCIP rebuild will actually run.
                                    if let Some(ref cb) = indexing_cb_ts {
                                        cb(true);
                                    }

                                    // The TypeScript path has no serve-side status
                                    // notifier yet, so only the general "Indexing"
                                    // label reflects it (via indexing_cb_ts).
                                    Self::run_full_rebuild_logged(
                                        indexer,
                                        &rp,
                                        &dp,
                                        &repo_label,
                                        "TypeScript",
                                        None,
                                    );

                                    // Clear "Indexing" regardless of outcome
                                    if let Some(ref cb) = indexing_cb_ts {
                                        cb(false);
                                    }
                                }
                            });
                        }
                    }
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
        cancel_token: &CancellationToken,
        embedding_pool: Option<&Arc<EmbeddingServicePool>>,
    ) -> Result<()> {
        use crate::output::set_quiet;

        let start = std::time::Instant::now();

        // Bail before touching any store if the repo was removed.
        Self::ensure_indexing_active(cancel_token)?;

        // One permit for the whole watcher batch: a batch can hold hundreds of
        // files, each re-embedded and followed by an HNSW rebuild, and every
        // repo's watcher would otherwise run one at the same time.
        let _job_permit = match stores.acquire_job_permit_or_cancel(cancel_token).await {
            crate::limits::JobAcquire::Ready(permit) => permit,
            crate::limits::JobAcquire::Cancelled => {
                return Err(anyhow::anyhow!("indexing cancelled"));
            }
        };

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
            store.build_index().await?;
        }

        // Then, index modified/new files
        // Abort before the per-file index loop if cancellation landed during the
        // removal phase above.
        Self::ensure_indexing_active(cancel_token)?;
        for file_path in &files_to_index {
            debug!("📄 Indexing: {}", file_path.display());
            // Redrive the per-file attempt on the transient Windows FTS
            // commit race — the same protection force reindex already has.
            if let Err(e) = Self::retry_on_transient_fts(&Self::FTS_TRANSIENT_BACKOFFS, || async {
                Self::index_single_file(codebase_path, file_path, stores, embedding_pool).await
            })
            .await
            {
                warn!("⚠️  Failed to index {}: {}", file_path.display(), e);
            }
        }

        // Disable quiet mode after batch processing is complete
        set_quiet(false);

        // Track changes for dashboard/TUI
        let batch_changes = (files_to_index.len() + files_to_remove.len()) as u64;
        if batch_changes > 0 {
            stores
                .changes_count
                .fetch_add(batch_changes, std::sync::atomic::Ordering::Relaxed);
        }

        let elapsed = start.elapsed();
        info!(
            "✅ Batch complete: {} indexed, {} removed in {:.2}s",
            files_to_index.len(),
            files_to_remove.len(),
            elapsed.as_secs_f64()
        );

        // Persist chunk/file counts in metadata.json for status(projects)
        {
            let vs = stores.vector_store.read().await;
            if let Ok(stats) = vs.stats().await {
                super::update_metadata_stats(db_path, stats.total_chunks, stats.total_files);
            }
        }

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
        cancel_token: &CancellationToken,
        embedding_pool: Option<&Arc<EmbeddingServicePool>>,
    ) -> Result<()> {
        use crate::cache::FileMetaStore;
        use crate::file::FileWalker;
        use crate::output::set_quiet;

        let start = std::time::Instant::now();
        set_quiet(true);

        // Abort before the filesystem walk if the repo was already removed.
        Self::ensure_indexing_active(cancel_token)?;

        // One permit for the whole branch-refresh pass (walk, delete, re-index).
        let _job_permit = match stores.acquire_job_permit_or_cancel(cancel_token).await {
            crate::limits::JobAcquire::Ready(permit) => permit,
            crate::limits::JobAcquire::Cancelled => {
                return Err(anyhow::anyhow!("indexing cancelled"));
            }
        };

        let result: Result<()> = async {
            // Phase 1: Discover current files on disk.
            // `walk()` is synchronous + I/O-heavy — offload off the async executor.
            let codebase = codebase_path.to_path_buf();
            let (files, stats) =
                tokio::task::spawn_blocking(move || FileWalker::new(codebase).walk())
                    .await
                    .map_err(|e| anyhow::anyhow!("file walk task panicked: {}", e))??;
            info!(
                "🔍 Branch refresh: discovered {} indexable files ({} skipped)",
                files.len(),
                stats.total_files - stats.indexable_files
            );

            // Phase 2: Load file metadata and analyze changes
            let metadata_path = db_path.join("metadata.json");
            if !metadata_path.exists() {
                info!("⚠️ No metadata.json found, skipping branch refresh");
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
            if super::key_migration::repair_legacy_index_shared(
                codebase_path,
                &mut file_meta_store,
                stores,
            )
            .await?
            {
                file_meta_store.save(db_path)?;
            }

            // Find files that need re-indexing (new or content changed)
            let mut files_to_reindex: Vec<PathBuf> = Vec::new();
            let mut chunks_to_delete: Vec<u32> = Vec::new();

            for file_info in &files {
                let key = crate::cache::storage_key(&file_info.path, codebase_path);
                let (needs_reindex, old_chunk_ids) =
                    file_meta_store.check_file(&file_info.path, &key)?;
                if needs_reindex {
                    chunks_to_delete.extend(old_chunk_ids);
                    files_to_reindex.push(file_info.path.clone());
                }
            }

            // Find files that were deleted (tracked in metadata but not on disk)
            let mut deleted_files = file_meta_store.find_deleted_files(codebase_path);

            // Phase 3.6: heal mixed generations on LIVE files. A failed FTS
            // commit used to leave the failed attempt's vector chunks
            // untracked (never in file_meta) while the tracked generation's
            // ids were already deleted from the vector store — search (FTS)
            // and get_chunk (vector) then disagreed (the field-reported stale-index
            // symptom). Delete untracked vector ids; queue tracked files
            // whose ids are missing from the vector store for Phase 4.
            {
                let vs_file_chunks = {
                    let vstore = stores.vector_store.read().await;
                    vstore.get_chunks_by_file().await?
                };
            let tracked: std::collections::HashSet<u32> =
                file_meta_store.tracked_chunk_ids();
            let is_local = stores.vector_store.read().await.local().is_some();
            let mut untracked_ids: Vec<u32> = Vec::new();
            for (vs_path, chunk_ids) in &vs_file_chunks {
                if !codebase_path.join(vs_path).exists() {
                    if !is_local && !deleted_files.iter().any(|(path, _)| path == vs_path) {
                        deleted_files.push((vs_path.clone(), chunk_ids.clone()));
                    }
                    continue; // ghost files: Phase 3.5 owns them
                }
                if is_local {
                    untracked_ids.extend(chunk_ids.iter().filter(|id| !tracked.contains(id)));
                } else if chunk_ids.iter().any(|id| !tracked.contains(id)) {
                    let path = codebase_path.join(vs_path);
                    if !files_to_reindex.contains(&path) {
                        files_to_reindex.push(path);
                    }
                }
            }
            let mut heal_files: Vec<PathBuf> = Vec::new();
            for key in file_meta_store.tracked_files() {
                let Some(meta_ids) = file_meta_store.file_chunk_ids(key) else {
                    continue;
                };
                if !codebase_path.join(key).exists() {
                    continue;
                }
                let missing = meta_ids
                    .iter()
                    .filter(|id| {
                        !vs_file_chunks
                            .get(key)
                            .is_some_and(|v| v.contains(id))
                    })
                    .count();
                if missing > 0 {
                    let p = codebase_path.join(key);
                    if !files_to_reindex.contains(&p) {
                        heal_files.push(p);
                    }
                }
            }
            if !untracked_ids.is_empty() || !heal_files.is_empty() {
                info!(
                    "🧹 Healing mixed generations: {} untracked vector chunks, {} live files missing tracked chunks",
                    untracked_ids.len(),
                    heal_files.len()
                );
                if !untracked_ids.is_empty() {
                    {
                        let mut vstore = stores.vector_store.write().await;
                        vstore.delete_chunks(&untracked_ids).await?;
                    }
                    {
                        let vector_store = Arc::clone(&stores.vector_store);
                        tokio::task::spawn_blocking(move || {
                            let mut vstore = vector_store.blocking_write();
                            vstore.local_mut().map_or(Ok(()), |local| local.build_index())
                        })
                        .await
                        .map_err(|e| anyhow::anyhow!("build_index task panicked: {}", e))??;
                    }
                    {
                        let mut fstore = stores.fts_store.write().await;
                        for &cid in &untracked_ids {
                            let _ = fstore.delete_chunk(cid);
                        }
                        fstore.commit()?;
                    }
                }
                files_to_reindex.extend(heal_files);
            }
        }

            if files_to_reindex.is_empty() && deleted_files.is_empty() {
                info!("✅ Branch refresh: index is up to date, no changes needed");
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
                    if let Some(local) = vstore.local_mut() { local.delete_chunks(&chunks_to_delete)?; }
                    else {
                        for (path, _) in &deleted_files { vstore.delete_file(path).await?; }
                    }
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
            let mut deleted_count = deleted_files.len();
            for (file_path, _chunk_ids) in &deleted_files {
                file_meta_store.remove_file(file_path);
            }

            // Save metadata after deletions (before re-indexing, since
            // index_single_file loads its own fresh copy per file)
            file_meta_store.save(db_path)?;

            // Rebuild vector index after FileMetaStore-based deletions.
            // `build_index()` is CPU-heavy — run it on `spawn_blocking`.
            {
                let vector_store = Arc::clone(&stores.vector_store);
                tokio::task::spawn_blocking(move || {
                    let mut vstore = vector_store.blocking_write();
                    vstore.local_mut().map_or(Ok(()), |local| local.build_index())
                })
                .await
                .map_err(|e| anyhow::anyhow!("build_index task panicked: {}", e))??;
            }

            // Phase 3.5: VectorStore-direct orphan cleanup
            // FileMetaStore may not track all ghost chunks (from pre-fix indexing runs).
            // Directly scan the VectorStore for chunks referencing files not on disk.
            {
                let vstore = stores.vector_store.read().await;
                let vs_file_chunks = vstore.get_chunks_by_file().await?;
                drop(vstore); // Release read lock before potential write

                let mut orphan_chunk_ids: Vec<u32> = Vec::new();
                let mut orphan_file_count = 0usize;

                for (vs_path, chunk_ids) in &vs_file_chunks {
                    // Stored chunk paths are project-relative; resolve against
                    // the codebase root for the on-disk existence check.
                    if !codebase_path.join(vs_path).exists() {
                        orphan_chunk_ids.extend(chunk_ids);
                        orphan_file_count += 1;
                    }
                }

                if !orphan_chunk_ids.is_empty() {
                    info!(
                        "🧹 Found {} orphan chunks across {} ghost files in VectorStore (not tracked by FileMetaStore)",
                        orphan_chunk_ids.len(),
                        orphan_file_count
                    );

                    // Delete orphan chunks from VectorStore, then rebuild the
                    // HNSW index off the async executor (CPU-heavy).
                    {
                        let mut vstore = stores.vector_store.write().await;
                        vstore.delete_chunks(&orphan_chunk_ids).await?;
                    }
                    {
                        let vector_store = Arc::clone(&stores.vector_store);
                        tokio::task::spawn_blocking(move || {
                            let mut vstore = vector_store.blocking_write();
                            vstore.local_mut().map_or(Ok(()), |local| local.build_index())
                        })
                        .await
                        .map_err(|e| anyhow::anyhow!("build_index task panicked: {}", e))??;
                    }

                    // Delete orphan chunks from FtsStore
                    {
                        let mut fstore = stores.fts_store.write().await;
                        for &cid in &orphan_chunk_ids {
                            let _ = fstore.delete_chunk(cid);
                        }
                        fstore.commit()?;
                    }

                    info!(
                        "✅ Cleaned {} orphan chunks from {} ghost files in VectorStore",
                        orphan_chunk_ids.len(),
                        orphan_file_count
                    );

                    deleted_count += orphan_file_count;
                }
            }

            // Phase 4: Re-index changed/new files
            // Abort before the per-file re-index loop if cancellation arrived
            // during the deletion/orphan-cleanup phases above.
            Self::ensure_indexing_active(cancel_token)?;
            let reindex_count = files_to_reindex.len();
            for file_path in &files_to_reindex {
                // Redrive on the transient Windows FTS commit race — the
                // same protection force reindex already has.
                if let Err(e) =
                    Self::retry_on_transient_fts(&Self::FTS_TRANSIENT_BACKOFFS, || async {
                        Self::index_single_file(codebase_path, file_path, stores, embedding_pool)
                            .await
                    })
                    .await
                {
                    warn!("⚠️  Failed to re-index {}: {}", file_path.display(), e);
                }
            }

            let elapsed = start.elapsed();
            info!(
                "✅ Branch refresh complete: {} re-indexed, {} stale removed in {:.2}s",
                reindex_count,
                deleted_count,
                elapsed.as_secs_f64()
            );

            Ok(())
        }
        .await;
        set_quiet(false);
        result
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
        super::index_quiet(
            Some(path.to_path_buf()),
            false,
            false,
            CancellationToken::new(),
        )
        .await?;

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
        embedding_pool: Option<&Arc<EmbeddingServicePool>>,
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

        // Chunk the file under its project-relative storage key so chunk
        // metadata stays machine-portable.
        let chunker = SemanticChunker::new(100, 4000, 2);
        let rel_key = crate::cache::storage_key(file_path, codebase_path);
        let chunks = chunker.chunk_file(std::path::Path::new(&rel_key), &content)?;

        if chunks.is_empty() {
            let ids = stores
                .vector_store
                .read()
                .await
                .get_chunks_by_file()
                .await?
                .remove(&rel_key)
                .unwrap_or_default();
            stores
                .vector_store
                .write()
                .await
                .delete_file(&rel_key)
                .await?;
            let mut fts = stores.fts_store.write().await;
            fts.delete_by_path(&rel_key)?;
            for id in ids {
                fts.delete_chunk(id)?;
            }
            fts.commit()?;
            let (model, dimensions) = Self::resolve_embed_model(&db_path)?;
            let mut metadata =
                FileMetaStore::load_or_create(&db_path, model.short_name(), dimensions)?;
            metadata.update_file(file_path, &rel_key, Vec::new())?;
            metadata.save(&db_path)?;
            return Ok(());
        }

        debug!(
            "Created {} chunks for file: {}",
            chunks.len(),
            file_path.display()
        );

        // Resolve the embedding model recorded for this index BEFORE embedding.
        // The live watcher path must re-embed changed files with the SAME model
        // the index was created with — using the hardcoded default here would
        // write 384d vectors into a non-384d index and corrupt it.
        let (embed_model, dimensions) = Self::resolve_embed_model(&db_path)?;
        let model_name = embed_model.short_name();

        // Generate embeddings. Reuse the serve pool's service when available:
        // the pool already holds the persistent cache's LMDB env open, so a
        // fresh EmbeddingService here is refused by the process-global
        // registry and would degrade this path to cache-less re-embedding.
        let embedded_chunks = match embed_service_choice(embedding_pool) {
            EmbeddingSource::Pool(pool) => {
                // The FSW paths hold a job permit (`process_batch_with_stores`
                // / `refresh_index_with_stores`); interleave the model lock per
                // mini-batch so a concurrent query embedding is not stuck
                // behind this file.
                pool.embed_chunks_yielding(embed_model, chunks, std::time::Duration::ZERO)?
            }
            EmbeddingSource::Fresh => {
                let cache_dir = crate::constants::get_global_models_cache_dir()?;
                let mut embedding_service =
                    EmbeddingService::with_cache_dir(embed_model, Some(cache_dir.as_path()))?;
                embedding_service.embed_chunks(chunks)?
            }
        };

        // Local replacement deletes tracked chunks before insertion.
        // PostgreSQL replaces the complete file in its storage transaction.
        if stores.vector_store.read().await.local().is_some() {
            let mut file_meta_store =
                FileMetaStore::load_or_create(&db_path, model_name, dimensions)?;
            if let Some(old_meta) = file_meta_store.remove_file(&rel_key) {
                let old_ids = old_meta.chunk_ids;
                if !old_ids.is_empty() {
                    debug!(
                        "Removing {} stale chunks before re-index: {}",
                        old_ids.len(),
                        file_path.display()
                    );
                    // Vector store — batch delete
                    {
                        let mut store = stores.vector_store.write().await;
                        if let Some(local) = store.local_mut() {
                            local.delete_chunks(&old_ids)?;
                        }
                    }
                    // FTS store — per-chunk delete + commit
                    {
                        let mut fts_store = stores.fts_store.write().await;
                        for id in &old_ids {
                            fts_store.delete_chunk(*id)?;
                        }
                        fts_store.commit()?;
                    }
                }
                // Persist the removal so the stale ids are gone from metadata
                file_meta_store.save(&db_path)?;
            }
        }

        // Use shared stores with write lock
        let chunk_ids = {
            let mut store = stores.vector_store.write().await;
            let chunk_ids = store
                .insert_chunks_with_ids(embedded_chunks.clone())
                .await?;
            // Rebuild the vector index after inserting new chunks
            store.build_index().await?;
            chunk_ids
        };

        // Publish FTS before file metadata. A PostgreSQL commit is retained
        // on publication failure; mismatched metadata queues the file for retry.
        if let Err(e) = Self::publish_file_chunks(
            &db_path,
            file_path,
            stores,
            &rel_key,
            &embedded_chunks,
            &chunk_ids,
            model_name,
            dimensions,
        )
        .await
        {
            Self::rollback_vector_insert(stores, &chunk_ids).await;
            return Err(e);
        }

        info!(
            "✅ Indexed {} ({} chunks)",
            file_path.display(),
            embedded_chunks.len()
        );

        Ok(())
    }

    /// Remove failed local inserts and FTS documents. Committed PostgreSQL
    /// replacements remain available for retrying local publication.
    async fn rollback_vector_insert(stores: &SharedStores, chunk_ids: &[u32]) {
        {
            let mut store = stores.vector_store.write().await;
            if let Some(local) = store.local_mut() {
                if let Err(error) = local.delete_chunks(chunk_ids) {
                    warn!("Vector rollback failed: {error:#}");
                }
            }
        }
        {
            let mut fts_store = stores.fts_store.write().await;
            for id in chunk_ids {
                let _ = fts_store.delete_chunk(*id);
            }
            let _ = fts_store.commit();
        }
    }

    /// FTS insert + commit + file_meta update for one file's fresh chunks —
    /// split from [`Self::index_single_file`] so the caller can roll the
    /// vector insert back when any of these steps fails.
    #[allow(clippy::too_many_arguments)]
    async fn publish_file_chunks(
        db_path: &Path,
        file_path: &Path,
        stores: &SharedStores,
        rel_key: &str,
        embedded_chunks: &[crate::embed::EmbeddedChunk],
        chunk_ids: &[u32],
        model_name: &str,
        dimensions: usize,
    ) -> Result<()> {
        // Add to FTS with write lock
        {
            let mut fts_store = stores.fts_store.write().await;
            fts_store.delete_by_path(rel_key)?;
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
        let mut file_meta_store =
            crate::cache::FileMetaStore::load_or_create(db_path, model_name, dimensions)?;
        file_meta_store.update_file(file_path, rel_key, chunk_ids.to_vec())?;
        file_meta_store.save(db_path)?;

        Ok(())
    }

    /// Remove a file from the index using shared stores (for FSW delete events).
    /// This version uses the shared stores to avoid LMDB conflicts.
    async fn remove_file_from_index_with_stores(
        codebase_path: &Path,
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
        let key = crate::cache::storage_key(file_path, codebase_path);
        let remote_ids = if stores.vector_store.read().await.backend() == "postgres" {
            let mut store = stores.vector_store.write().await;
            let ids = store
                .get_chunks_by_file()
                .await?
                .remove(&key)
                .unwrap_or_default();
            store.delete_file(&key).await?;
            ids
        } else {
            Vec::new()
        };
        let meta = file_meta_store.remove_file(&key);
        let chunk_ids = match meta {
            Some(m) if !m.chunk_ids.is_empty() => m.chunk_ids,
            Some(_) if !remote_ids.is_empty() => remote_ids,
            Some(_) => {
                debug!("No chunks to remove for file: {}", file_path.display());
                file_meta_store.save(db_path)?;
                return Ok(());
            }
            None if !remote_ids.is_empty() => remote_ids,
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
                store.delete_chunks(&[*chunk_id]).await?;
            }
        }

        // Delete from FTS with write lock
        {
            let mut fts_store = stores.fts_store.write().await;
            fts_store.delete_by_path(&key)?;
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

    /// The watcher must classify every restore input by file NAME,
    /// case-insensitively (NuGet honours lowercase nuget.config on
    /// case-sensitive filesystems), and must not classify lookalikes.
    #[test]
    fn restore_input_classification_table() {
        let cases = [
            ("Directory.Packages.props", true),
            ("directory.packages.props", true),
            ("Directory.Build.props", true),
            ("Directory.Build.targets", true),
            ("nuget.config", true),
            ("NuGet.CONFIG", true),
            ("global.json", true),
            // Lookalikes that must NOT trigger a full rebuild.
            ("Global.json.bak", false),
            ("MyNuGet.config", false),
            ("Program.cs", false),
            ("Directory.Packages.props.bak", false),
            ("packages.props", false),
        ];
        for (name, expected) in cases {
            assert_eq!(
                is_csharp_restore_input(Path::new(name)),
                expected,
                "file name: {name}"
            );
            // Same answer through a directory-bearing path.
            let nested = Path::new("src").join(name);
            assert_eq!(
                is_csharp_restore_input(&nested),
                expected,
                "nested path: {nested:?}"
            );
        }
    }

    /// The per-event decision the FSW arms rely on: .cs feeds the
    /// per-project incremental, a restore input the FULL-rebuild debounce,
    /// .ts/.tsx the TypeScript one — this is the wiring the deliverable
    /// behaviour depends on, pinned without driving the async loop.
    #[test]
    fn symbol_rebuild_event_classification_table() {
        use SymbolRebuildTrigger::*;
        let cases = [
            ("src/Acme.Core.cs", CSharpIncremental),
            ("Acme.cs", CSharpIncremental),
            ("Directory.Packages.props", CSharpRestoreInput),
            ("nuget.config", CSharpRestoreInput),
            ("Directory.Build.targets", CSharpRestoreInput),
            ("global.json", CSharpRestoreInput),
            ("src/app.ts", TypeScript),
            ("src/app.tsx", TypeScript),
            ("src/app.mts", TypeScript),
            ("README.md", None),
            ("Cargo.toml", None),
            ("Directory.Packages.props.bak", None),
        ];
        for (path, expected) in cases {
            assert_eq!(
                classify_symbol_rebuild_event(Path::new(path)),
                expected,
                "path: {path}"
            );
        }
    }

    /// The grouped incremental rebuild's terminal-signal ordering: a real
    /// failure beats an abandonment (exhausted groups leave the previous
    /// index live — warning — but a genuine failure must render red), and
    /// an abandonment beats success. Pins the decision the watcher path
    /// relies on, without driving the async loop — the same
    /// extract-and-table-test shape as the serve layer's classifier.
    #[test]
    fn grouped_rebuild_signal_priority_table() {
        use SymbolRebuildSignal::*;
        let real = "MDB_BAD_VALSIZE: put failed".to_string();
        let exhausted = "dotnet restore did not finish within 3 attempts".to_string();
        let warnings = vec!["MSBuild advisory".to_string()];
        let cases = [
            (
                "real failure beats abandonment",
                Some(real.clone()),
                Some(exhausted.clone()),
                Failed(real.clone()),
            ),
            (
                "abandonment beats success",
                None,
                Some(exhausted.clone()),
                Abandoned(exhausted.clone()),
            ),
            (
                "clean run keeps the surviving warnings",
                None,
                None,
                Succeeded {
                    index_warnings: warnings.clone(),
                },
            ),
        ];
        for (label, last_error, last_abandon, expected) in cases {
            assert_eq!(
                grouped_rebuild_signal(last_error, last_abandon, warnings.clone()),
                expected,
                "case: {label}"
            );
        }
    }

    /// Dropping the last `Arc<SharedStores>` must release every handle inside
    /// the DB directory — the precondition `index rm`'s delete path depends on.
    ///
    /// Pins the Windows manifestation of a heed 0.20 leak fixed in
    /// `TrackedEnv::drop`: the `OPENED_ENV` cache entry held a strong `Env`
    /// clone, so `mdb_env_close` never ran after a plain drop and
    /// `data.mdb`/`lock.mdb` stayed locked for the life of the process —
    /// `serve::tests::index_rm_deletes_db_while_serve_holds_real_lmdb_env`
    /// failed deterministically on os error 32 through the whole 60 s retry
    /// budget with an EMPTY LMDB registry (the holder was invisible to it).
    /// This is the serve-free, instant version of that acceptance test.
    #[test]
    fn sharedstores_drop_releases_db_dir_for_deletion() {
        let tmp = tempdir().unwrap();
        let db = tmp.path().join(".codesearch.db");
        let stores = SharedStores::new(&db, 384).expect("open SharedStores");
        assert!(
            !crate::lmdb_registry::open_holders_under(&db).is_empty(),
            "holder must be visible while SharedStores lives"
        );
        drop(stores);
        assert!(
            crate::lmdb_registry::open_holders_under(&db).is_empty(),
            "registry must drain after the last Arc<SharedStores> drops"
        );
        std::fs::remove_dir_all(&db)
            .expect("db dir must be deletable after SharedStores drops (no leaked LMDB handles)");
    }

    /// The pool arm must be taken exactly when a pool is present, carrying
    /// that same pool; `None` must select a fresh service. Pins the arm
    /// selection both embed call sites share — no model is loaded here
    /// (`EmbeddingServicePool::new` is a plain constructor).
    #[test]
    fn embed_service_choice_selects_pool_when_present_and_fresh_when_none() {
        let pool = Arc::new(EmbeddingServicePool::new(None));
        match embed_service_choice(Some(&pool)) {
            EmbeddingSource::Pool(p) => assert!(Arc::ptr_eq(&p, &pool)),
            EmbeddingSource::Fresh => panic!("Some(pool) must select Pool, got Fresh"),
        }
        assert!(matches!(embed_service_choice(None), EmbeddingSource::Fresh));
    }

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
            vector_store: Arc::new(RwLock::new(ChunkStore::Local(
                VectorStore::new(db_path, dimensions).unwrap(),
            ))),
            fts_store: Arc::new(RwLock::new(FtsStore::new_with_writer(db_path).unwrap())),
            writer_lock: None,
            readonly: false,
            changes_count: std::sync::atomic::AtomicU64::new(0),
            job_gate: None,
        }
    }

    #[test]
    fn shared_stores_new_creates_missing_db_directory() {
        // Regression: a brand-new repo's `.codesearch.db` directory does not exist
        // yet when the serve auto-register path (POST /repos) opens the stores.
        // SharedStores::new() must create it and acquire the writer lock, NOT fail
        // with "Database is locked by another process".
        let temp = tempdir().unwrap();
        // Intentionally do NOT create this directory — it must not exist.
        let db_path = temp.path().join("brand_new").join(DB_DIR_NAME);
        assert!(!db_path.exists(), "precondition: db dir must not exist yet");

        let stores = SharedStores::new(&db_path, 384)
            .expect("SharedStores::new must succeed on a non-existent db_path");

        assert!(db_path.exists(), "db directory should have been created");
        assert!(!stores.readonly, "should be opened in read-write mode");
        assert!(
            db_path.join(WRITER_LOCK_FILE).exists(),
            "writer lock file should have been created"
        );
    }

    /// `SharedStores::new` must keep the FTS writer lazy: an eager tantivy
    /// writer parked its pipeline threads (workers + segment updater + merge
    /// pool) for every warm repo for as long as serve held the store — a
    /// 500-repo pool was observed with ~4 500 resident OS threads. The
    /// writer must materialize only on the first actual write, and the
    /// Manual-policy reader must see our own commit without any watcher.
    #[tokio::test]
    async fn shared_stores_new_keeps_fts_writer_lazy_until_first_write() {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join(DB_DIR_NAME);
        let stores = SharedStores::new(&db_path, 384).unwrap();
        let tantivy_lock = db_path.join("fts").join(".tantivy-writer.lock");
        assert!(
            !tantivy_lock.exists(),
            "SharedStores::new must not materialize the tantivy writer"
        );

        {
            let mut fts = stores.fts_store.write().await;
            fts.add_chunk(7, "shared lazy writer probe", "probe.rs", None, "block")
                .unwrap();
            fts.commit().unwrap();
        }

        assert!(
            tantivy_lock.exists(),
            "first FTS write must materialize the writer and its lockfile"
        );
        let fts = stores.fts_store.read().await;
        let hits = fts.search("shared lazy", 10, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk_id, 7);
    }

    #[test]
    fn acquire_writer_lock_succeeds_for_missing_directory() {
        // acquire_writer_lock must create the db directory before placing the lock
        // file, so a fresh repo path yields a real lock rather than None.
        let temp = tempdir().unwrap();
        let db_path = temp.path().join("nested").join(DB_DIR_NAME);
        assert!(!db_path.exists());

        let lock = acquire_writer_lock(&db_path);
        assert!(
            lock.is_some(),
            "acquire_writer_lock should create the dir and acquire the lock"
        );
        assert!(db_path.join(WRITER_LOCK_FILE).exists());
    }

    /// The chunk-cap predicate is the batch-splitting decision; pin its edges
    /// here because the loop that uses it runs inside `spawn_blocking` and
    /// would otherwise need the ONNX embedding model.
    #[test]
    fn chunk_cap_predicate_table() {
        let cases = [
            // (consumed, accumulated_chunks, cap, expected)
            (0, 10_000, 5_000, false), // first file of a batch always proceeds
            (1, 4_999, 5_000, false),  // below the cap
            (1, 5_000, 5_000, true),   // exactly at the cap
            (1, 5_001, 5_000, true),   // one file overshot the cap
            (7, 10_000, 0, false),     // cap disabled
            (1, 0, 0, false),          // cap disabled, nothing accumulated
        ];
        for (consumed, accumulated, cap, expected) in cases {
            assert_eq!(
                chunk_cap_reached(consumed, accumulated, cap),
                expected,
                "chunk_cap_reached(consumed={consumed}, accumulated={accumulated}, cap={cap})"
            );
        }
    }

    /// Standalone CLI / `codesearch mcp` stores carry no gate and must keep
    /// their previous (unbounded) behaviour.
    #[tokio::test]
    async fn acquire_job_permit_is_none_for_ungated_stores() {
        let temp = tempdir().unwrap();
        let stores = create_test_stores(&temp.path().join("db"), 4).await;
        assert!(stores.acquire_job_permit().await.is_none());
    }

    /// Cancellation-aware acquisition: ungated stores must distinguish
    /// "proceed without a gate" from "cancelled", and a waiter cancelled while
    /// the only slot is held must abort promptly instead of parking.
    #[tokio::test]
    async fn acquire_job_permit_or_cancel_distinguishes_cancel_from_ungated() {
        let temp = tempdir().unwrap();
        let stores = create_test_stores(&temp.path().join("db"), 4).await;

        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(matches!(
            stores.acquire_job_permit_or_cancel(&cancelled).await,
            crate::limits::JobAcquire::Cancelled
        ));
        assert!(matches!(
            stores
                .acquire_job_permit_or_cancel(&CancellationToken::new())
                .await,
            crate::limits::JobAcquire::Ready(None)
        ));

        // Gated: a waiter cancelled while the only slot is held aborts.
        let gate = crate::limits::JobGate::with_config(1, 0);
        let gated = Arc::new(
            create_test_stores(&temp.path().join("db2"), 4)
                .await
                .with_job_gate(Arc::clone(&gate)),
        );
        let held = gate.acquire().await;
        let waiter = CancellationToken::new();
        let canceller = waiter.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            canceller.cancel();
        });
        assert!(matches!(
            gated.acquire_job_permit_or_cancel(&waiter).await,
            crate::limits::JobAcquire::Cancelled
        ));
        drop(held);
    }

    /// A gated refresh must (a) wait while the only slot is held and (b)
    /// release the slot when it finishes. The repo has NO changed files, so the
    /// refresh completes without loading the ONNX model.
    #[tokio::test]
    async fn gated_refresh_waits_for_a_free_slot_and_releases_it() {
        let temp = tempdir().unwrap();
        let codebase_path = temp.path().join("codebase");
        let db_path = temp.path().join("db");
        std::fs::create_dir_all(&codebase_path).unwrap();
        std::fs::create_dir_all(&db_path).unwrap();
        create_metadata_json(&db_path, 4);

        let gate = crate::limits::JobGate::with_config(1, 0);
        let held = gate.acquire().await;
        let stores = Arc::new(
            create_test_stores(&db_path, 4)
                .await
                .with_job_gate(Arc::clone(&gate)),
        );

        let mut refresh = tokio::spawn({
            let stores = Arc::clone(&stores);
            let codebase_path = codebase_path.clone();
            let db_path = db_path.clone();
            async move {
                IndexManager::perform_incremental_refresh_with_stores(
                    &codebase_path,
                    &db_path,
                    &stores,
                    &CancellationToken::new(),
                    None,
                    None,
                )
                .await
            }
        });

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(150), &mut refresh)
                .await
                .is_err(),
            "the refresh must wait while the only job slot is held"
        );

        drop(held);
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), refresh)
            .await
            .expect("refresh must complete once the slot is free")
            .expect("refresh task must not panic");
        assert!(result.is_ok(), "refresh failed: {:?}", result.err());

        // The permit was released: the gate can be acquired again immediately.
        let _again = tokio::time::timeout(std::time::Duration::from_secs(1), gate.acquire())
            .await
            .expect("the refresh must release its job permit");
    }

    /// The transient-FTS retry redrives the WHOLE pass, one extra pass per
    /// backoff entry: a transient-shaped error is retried (the field case —
    /// the first pass fails on the AV holding a new segment file, the retry
    /// lands), a non-transient error is returned without a second attempt,
    /// and a transient failure that outlives the last backoff gives up —
    /// bounded, never a loop.
    #[tokio::test]
    async fn retry_on_transient_fts_redrives_per_backoff_then_gives_up() {
        let ms = [
            std::time::Duration::from_millis(1),
            std::time::Duration::from_millis(1),
        ];

        // Transient once → retried, succeeds, exactly two attempts.
        let mut attempts = 0u32;
        let out: Result<u32> = IndexManager::retry_on_transient_fts(&ms, || {
            let done = attempts >= 1;
            attempts += 1;
            async move {
                if done {
                    Ok(1)
                } else {
                    Err(anyhow::anyhow!(
                        "FTS commit failed: Access is denied. (os error 5)"
                    ))
                }
            }
        })
        .await;
        assert_eq!(out.unwrap(), 1);
        assert_eq!(attempts, 2);

        // Non-transient → returned immediately, one attempt.
        let mut attempts = 0u32;
        let out: Result<()> = IndexManager::retry_on_transient_fts(&ms, || {
            attempts += 1;
            async { Err(anyhow::anyhow!("schema mismatch")) }
        })
        .await;
        assert!(out.is_err());
        assert_eq!(attempts, 1);

        // Transient three times (initial + two backoffs) → gives up after
        // the third attempt; a lock outliving both backoffs stays loud.
        let mut attempts = 0u32;
        let out: Result<()> = IndexManager::retry_on_transient_fts(&ms, || {
            attempts += 1;
            async { Err(anyhow::anyhow!("FTS commit failed: os error 32")) }
        })
        .await;
        assert!(out.is_err());
        assert_eq!(attempts, 3);

        // Transient twice then clean → the second backoff saves the pass.
        let mut attempts = 0u32;
        let out: Result<u32> = IndexManager::retry_on_transient_fts(&ms, || {
            let done = attempts >= 2;
            attempts += 1;
            async move {
                if done {
                    Ok(7)
                } else {
                    Err(anyhow::anyhow!("FTS commit failed: os error 5"))
                }
            }
        })
        .await;
        assert_eq!(out.unwrap(), 7);
        assert_eq!(attempts, 3);
    }

    /// The db directory must carry FILE_ATTRIBUTE_HIDDEN after a read-write
    /// create: the Search indexer skips hidden directories, which is the
    /// structural fix for the scanner race on fresh Tantivy segment files.
    #[cfg(windows)]
    #[test]
    fn shared_stores_marks_the_db_dir_hidden() {
        let dir = tempdir().unwrap();
        let db = dir.path().join(".codesearch.db");
        let _stores = SharedStores::new(&db, 2).unwrap();
        use std::os::windows::fs::MetadataExt;
        let attrs = std::fs::metadata(&db).unwrap().file_attributes();
        assert_ne!(
            attrs & 0x2,
            0,
            "the db dir must carry FILE_ATTRIBUTE_HIDDEN"
        );
    }

    /// Regression for the wrapper/inner split: `force_reindex_with_stores`
    /// holds the gate permit across the destructive clear and then calls the
    /// gate-free inner body. Calling the public wrapper again would re-acquire
    /// the only permit and hang forever under `CODESEARCH_INDEX_JOBS=1` (this
    /// test times out instead of hanging only because of the explicit
    /// `timeout`).
    #[tokio::test]
    async fn force_reindex_under_gate_does_not_reacquire_the_permit() {
        let temp = tempdir().unwrap();
        let codebase_path = temp.path().join("codebase");
        let db_path = temp.path().join("db");
        std::fs::create_dir_all(&codebase_path).unwrap();
        std::fs::create_dir_all(&db_path).unwrap();
        create_metadata_json(&db_path, 4);

        let gate = crate::limits::JobGate::with_config(1, 0);
        let stores = create_test_stores(&db_path, 4)
            .await
            .with_job_gate(Arc::clone(&gate));

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            IndexManager::force_reindex_with_stores(
                &codebase_path,
                &db_path,
                &stores,
                None,
                &CancellationToken::new(),
                None,
            ),
        )
        .await
        .expect("force reindex must not deadlock on its own job permit");

        assert!(result.is_ok(), "force reindex failed: {:?}", result.err());
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
            &CancellationToken::new(),
            None,
        )
        .await;

        assert!(
            result.is_ok(),
            "Should return Ok when no metadata.json exists"
        );
    }

    #[tokio::test]
    async fn cancellation_aborts_incremental_refresh_before_embedding() {
        // FINDINGS #3: an indexing pass must observe its cancellation token, not
        // run to completion on an alias that `remove_repo` is tearing down.
        //
        // A pre-cancelled token makes `perform_incremental_refresh_with_stores`
        // bail at its entry checkpoint — BEFORE the file walk, embedding-model
        // load, or any store mutation — even though the codebase HAS a changed
        // file that would otherwise trigger a full embed pass. This locks the
        // contract the in-flight cancel path (`remove_repo` -> `await_index_task`)
        // depends on.
        //
        // Finer mid-pass checkpoints (per-file inside the `spawn_blocking` embed
        // loop, between batches, before `build_index`) also exist, but reaching
        // them requires loading the ONNX embedding model, so a true mid-embed
        // interrupt is an `#[ignore]` integration test, omitted here.
        let temp = tempdir().unwrap();
        let codebase_path = temp.path().join("codebase");
        let db_path = temp.path().join("db");
        std::fs::create_dir_all(&codebase_path).unwrap();
        std::fs::create_dir_all(&db_path).unwrap();
        create_metadata_json(&db_path, 4);
        // A real source file so the change-detector WOULD find work to do.
        std::fs::write(codebase_path.join("a.txt"), "hello world").unwrap();

        let stores = create_test_stores(&db_path, 4).await;

        let token = CancellationToken::new();
        token.cancel(); // already cancelled -> must abort immediately

        let result = IndexManager::perform_incremental_refresh_with_stores(
            &codebase_path,
            &db_path,
            &stores,
            &token,
            None,
            None,
        )
        .await;

        let err = result.expect_err("pre-cancelled token must abort indexing");
        assert!(
            err.to_string().contains("cancelled"),
            "expected a cancellation error, got: {err}"
        );
    }

    #[tokio::test]
    #[ignore = "loads the ONNX embedding model (~90MB download on first run); \
                run with `cargo test -- --ignored mid_pass_cancellation`"]
    async fn mid_pass_cancellation_aborts_a_running_embed() {
        // FINDINGS #3 (mid-pass, not just entry): the entry-level test above only
        // proves a pre-cancelled token bails before work begins. This test lets a
        // REAL embed pass START (past the entry checkpoint, into ONNX inference),
        // confirms it is still running, and THEN cancels — proving the per-batch /
        // per-file / pre-build_index checkpoints abort a RUNNING long pass, not
        // merely one that never began. Uses `force_reindex_with_stores` so the
        // default model metadata is stamped correctly (a hand-written "test-model"
        // short name would fail model resolution before reaching the embed loop).
        let temp = tempdir().unwrap();
        let codebase_path = temp.path().join("codebase");
        let db_path = temp.path().join("db");
        std::fs::create_dir_all(&codebase_path).unwrap();
        std::fs::create_dir_all(&db_path).unwrap();
        let dims = ModelType::default().dimensions();
        let stores = create_test_stores(&db_path, dims).await;

        // A large corpus so the full pass spans multiple embed batches and takes
        // long enough to reliably still be running when we cancel. If this flakes
        // because the pass finishes first, bump the file count.
        for i in 0..600 {
            std::fs::write(
                codebase_path.join(format!("file_{i:03}.txt")),
                format!("document body number {i} with enough prose to be chunked\n"),
            )
            .unwrap();
        }

        let token = CancellationToken::new();
        let task_token = token.clone();
        let handle = tokio::spawn(async move {
            IndexManager::force_reindex_with_stores(
                &codebase_path,
                &db_path,
                &stores,
                None,
                &task_token,
                None,
            )
            .await
        });

        // Give the pass a head start so it is past the entry checkpoint and into
        // model load / embedding. 200ms is comfortably past the (microsecond)
        // entry check while leaving the bulk of a 600-file pass ahead.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !handle.is_finished(),
            "corpus too small: the pass completed before we could cancel — \
             increase the file count so the embed run outlasts the head start"
        );

        let cancel_start = std::time::Instant::now();
        token.cancel();

        let result = handle.await.expect("indexing task panicked");
        let cancel_latency = cancel_start.elapsed();

        // The pass must abort to a cancellation error, NOT complete Ok — this is
        // the assertion that fails if the post-entry checkpoints were missing.
        let err = result.expect_err("a running embed pass must abort to a cancellation error");
        assert!(
            err.to_string().contains("cancelled"),
            "expected a cancellation error, got: {err}"
        );
        // The checkpoint fires at the next batch/phase boundary, well before a
        // full uncancelled pass over 600 files would finish.
        assert!(
            cancel_latency < std::time::Duration::from_secs(30),
            "cancellation took too long to take effect: {cancel_latency:?}"
        );
    }

    #[tokio::test]
    async fn force_reindex_stamps_model_when_metadata_has_only_schema_version() {
        // Regression for the "model: unknown" worktree bug.
        //
        // When a repo is registered via `POST /repos` (the git-hook path), the
        // store is opened FIRST and `ensure_schema_version` pre-creates a
        // metadata.json containing ONLY `schema_version` — no model fields.
        // Before the fix, force_reindex's Step 0 saw the file already exists and
        // skipped the default-model stamp, so the index was left with no
        // `model_short_name`: every reader then showed `model: unknown` AND the
        // live-chunk-count fallback bailed on that string, making the index look
        // empty (agent falls back to grep). This test reproduces that exact
        // bootstrap state and asserts force_reindex now stamps the default model.
        let temp = tempdir().unwrap();
        let codebase_path = temp.path().join("codebase");
        let db_path = temp.path().join("db");
        std::fs::create_dir_all(&codebase_path).unwrap();
        std::fs::create_dir_all(&db_path).unwrap();

        // create_test_stores → VectorStore::new → ensure_schema_version, which
        // writes the real "schema_version only" metadata.json — the exact bug state.
        let dims = ModelType::default().dimensions();
        let stores = create_test_stores(&db_path, dims).await;

        // Precondition: the bootstrap wrote NO model field.
        let before = std::fs::read_to_string(db_path.join("metadata.json")).unwrap();
        let before_json: serde_json::Value = serde_json::from_str(&before).unwrap();
        assert!(
            before_json.get("model_short_name").is_none(),
            "precondition: schema-version bootstrap must not write a model, got: {before}"
        );

        // Empty codebase → perform_incremental_refresh returns before any
        // embedding, so this exercises Fix A (the Step-0 stamp) without loading
        // an ONNX model.
        IndexManager::force_reindex_with_stores(
            &codebase_path,
            &db_path,
            &stores,
            None,
            &CancellationToken::new(),
            None,
        )
        .await
        .expect("force reindex on empty codebase should succeed");

        let after = std::fs::read_to_string(db_path.join("metadata.json")).unwrap();
        let after_json: serde_json::Value = serde_json::from_str(&after).unwrap();
        assert_eq!(
            after_json.get("model_short_name").and_then(|v| v.as_str()),
            Some(ModelType::default().short_name()),
            "metadata.json must have the default model_short_name stamped, got: {after}"
        );
        assert_eq!(
            after_json.get("dimensions").and_then(|v| v.as_u64()),
            Some(dims as u64),
            "metadata.json must record the default model's dimensions, got: {after}"
        );
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
            .update_file(&ghost_file, "ghost.rs", vec![100, 101])
            .unwrap();
        file_meta.save(&db_path).unwrap();

        // Now delete the ghost file from disk — simulates branch switch
        std::fs::remove_file(&ghost_file).unwrap();

        // Verify precondition: ghost file IS tracked but NOT on disk
        let deleted_before = file_meta.find_deleted_files(&codebase_path);
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
            &CancellationToken::new(),
            None,
        )
        .await;

        assert!(result.is_ok(), "Refresh should succeed: {:?}", result);

        // Verify: reload FileMetaStore and confirm ghost entry is gone
        let reloaded = FileMetaStore::load_or_create(&db_path, "test-model", 4).unwrap();
        let deleted_after = reloaded.find_deleted_files(&codebase_path);
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
        file_meta
            .update_file(&ghost1, "ghost1.rs", vec![10, 11])
            .unwrap();
        file_meta
            .update_file(&ghost2, "ghost2.rs", vec![20, 21, 22])
            .unwrap();
        file_meta
            .update_file(&ghost3, "ghost3.rs", vec![30])
            .unwrap();
        file_meta.save(&db_path).unwrap();

        // Delete all ghost files
        std::fs::remove_file(&ghost1).unwrap();
        std::fs::remove_file(&ghost2).unwrap();
        std::fs::remove_file(&ghost3).unwrap();

        // Verify precondition
        let deleted_before = file_meta.find_deleted_files(&codebase_path);
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
            &CancellationToken::new(),
            None,
        )
        .await;

        assert!(result.is_ok(), "Refresh should succeed: {:?}", result);

        // All ghost entries should be removed
        let reloaded = FileMetaStore::load_or_create(&db_path, "test-model", 4).unwrap();
        let deleted_after = reloaded.find_deleted_files(&codebase_path);
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
        file_meta
            .update_file(&real_file, "main.rs", vec![1, 2])
            .unwrap();
        file_meta.save(&db_path).unwrap();

        let stores = create_test_stores(&db_path, 4).await;

        let result = IndexManager::refresh_index_with_stores(
            &codebase_path,
            &db_path,
            &stores,
            &CancellationToken::new(),
            None,
        )
        .await;

        assert!(result.is_ok(), "Refresh should succeed: {:?}", result);

        // Verify: real file entry should still be in FileMetaStore
        let reloaded = FileMetaStore::load_or_create(&db_path, "test-model", 4).unwrap();
        let deleted = reloaded.find_deleted_files(&codebase_path);
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
        file_meta
            .update_file(&real_file, "real.rs", vec![1, 2])
            .unwrap();
        file_meta
            .update_file(&ghost_file, "ghost.rs", vec![3, 4, 5])
            .unwrap();
        file_meta.save(&db_path).unwrap();

        // Delete ghost file — simulates branch switch removing it
        std::fs::remove_file(&ghost_file).unwrap();

        let stores = create_test_stores(&db_path, 4).await;

        let result = IndexManager::refresh_index_with_stores(
            &codebase_path,
            &db_path,
            &stores,
            &CancellationToken::new(),
            None,
        )
        .await;

        assert!(result.is_ok(), "Refresh should succeed: {:?}", result);

        // Verify: ghost is removed, real is preserved
        let reloaded = FileMetaStore::load_or_create(&db_path, "test-model", 4).unwrap();
        let deleted = reloaded.find_deleted_files(&codebase_path);
        assert!(
            deleted.is_empty(),
            "Ghost entry should be removed, real should remain. Found deleted: {:?}",
            deleted
        );

        // Verify the real file is still tracked by checking it doesn't need reindex
        let (needs_reindex, _chunk_ids) = reloaded.check_file(&real_file, "real.rs").unwrap();
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
        file_meta
            .update_file(&file1, "lib.rs", vec![1, 2, 3])
            .unwrap();
        file_meta
            .update_file(&file2, "util.rs", vec![4, 5])
            .unwrap();
        file_meta.save(&db_path).unwrap();

        // Delete ALL files — simulates switching to a branch with no source
        std::fs::remove_file(&file1).unwrap();
        std::fs::remove_file(&file2).unwrap();

        let stores = create_test_stores(&db_path, 4).await;

        let result = IndexManager::refresh_index_with_stores(
            &codebase_path,
            &db_path,
            &stores,
            &CancellationToken::new(),
            None,
        )
        .await;

        assert!(result.is_ok(), "Refresh should succeed: {:?}", result);

        // All entries should be cleaned
        let reloaded = FileMetaStore::load_or_create(&db_path, "test-model", 4).unwrap();
        let deleted = reloaded.find_deleted_files(&codebase_path);
        assert!(deleted.is_empty(), "All stale entries should be removed");
    }

    #[tokio::test]
    async fn test_incremental_refresh_up_to_date_is_noop() {
        // End-to-end smoke test for perform_incremental_refresh_with_stores after
        // the file walk was moved onto spawn_blocking. When every on-disk file is
        // already tracked in FileMetaStore (no changes, no deletions), the refresh
        // must walk the codebase off the async executor and return Ok early without
        // touching the embedder.
        let temp = tempdir().unwrap();
        let codebase_path = temp.path().join("codebase");
        let db_path = temp.path().join("db");
        std::fs::create_dir_all(&codebase_path).unwrap();
        std::fs::create_dir_all(&db_path).unwrap();

        create_metadata_json(&db_path, 4);

        // Write a real source file and record its metadata so check_file reports
        // "unchanged" — this drives the no-changes branch (no embedding required).
        let file = codebase_path.join("lib.rs");
        std::fs::write(&file, "pub fn lib_fn() {}").unwrap();

        let mut file_meta = FileMetaStore::new("test-model".to_string(), 4);
        // update_file hashes current on-disk content, so a subsequent check_file
        // on the unmodified file returns needs_reindex = false.
        file_meta.update_file(&file, "lib.rs", vec![1]).unwrap();
        file_meta.save(&db_path).unwrap();

        let stores = create_test_stores(&db_path, 4).await;

        // The heartbeat must be threaded through and invoked. Only the
        // start-of-refresh beat is reachable here: a batch with changed files
        // needs a real embedder, which no unit test spins up.
        let beats = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let heartbeat: IndexingHeartbeat = {
            let beats = Arc::clone(&beats);
            Arc::new(move || {
                beats.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
        };

        let result = IndexManager::perform_incremental_refresh_with_stores(
            &codebase_path,
            &db_path,
            &stores,
            &CancellationToken::new(),
            None,
            Some(&heartbeat),
        )
        .await;

        assert!(
            result.is_ok(),
            "Up-to-date incremental refresh should succeed: {:?}",
            result
        );
        assert!(
            beats.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "heartbeat must fire at least once per refresh"
        );

        // The tracked file must still be tracked and not flagged as deleted.
        let reloaded = FileMetaStore::load_or_create(&db_path, "test-model", 4).unwrap();
        assert!(
            reloaded.find_deleted_files(&codebase_path).is_empty(),
            "No files should be flagged deleted on an up-to-date refresh"
        );
    }

    #[tokio::test]
    async fn test_incremental_refresh_round_trips_relative_keys() {
        // Chunk/file-meta paths are stored RELATIVE to the project root so the
        // built DB is portable across machines. A file tracked under its
        // relative storage key must be seen as UNCHANGED (not re-indexed, not
        // deleted) by a refresh pass that re-walks the same codebase.
        let temp = tempdir().unwrap();
        let codebase_path = temp.path().join("codebase");
        let db_path = temp.path().join("db");
        std::fs::create_dir_all(&codebase_path).unwrap();
        std::fs::create_dir_all(&db_path).unwrap();

        create_metadata_json(&db_path, 4);

        let file = codebase_path.join("lib.rs");
        std::fs::write(&file, "pub fn lib_fn() {}").unwrap();

        let mut file_meta = FileMetaStore::new("test-model".to_string(), 4);
        let key = crate::cache::storage_key(&file, &codebase_path);
        assert_eq!(key, "lib.rs", "storage key must be project-relative");
        file_meta.update_file(&file, &key, vec![1]).unwrap();
        file_meta.save(&db_path).unwrap();

        let stores = create_test_stores(&db_path, 4).await;

        let result = IndexManager::perform_incremental_refresh_with_stores(
            &codebase_path,
            &db_path,
            &stores,
            &CancellationToken::new(),
            None,
            None,
        )
        .await;

        assert!(
            result.is_ok(),
            "Second pass over unchanged files must succeed: {:?}",
            result
        );

        let reloaded = FileMetaStore::load_or_create(&db_path, "test-model", 4).unwrap();
        assert!(
            reloaded.is_tracked("lib.rs"),
            "Relative key must survive the refresh round-trip, keys: {:?}",
            reloaded.tracked_files().collect::<Vec<_>>()
        );
        assert!(
            !reloaded
                .tracked_files()
                .any(|k| std::path::Path::new(k).is_absolute()),
            "No stored key may be absolute, keys: {:?}",
            reloaded.tracked_files().collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn legacy_repair_never_sweeps_without_a_migration() {
        use crate::chunker::{Chunk, ChunkKind};
        use crate::embed::EmbeddedChunk;

        let temp = tempdir().unwrap();
        let codebase_path = temp.path().join("codebase");
        let db_path = temp.path().join("db");
        std::fs::create_dir_all(&codebase_path).unwrap();
        std::fs::create_dir_all(&db_path).unwrap();
        create_metadata_json(&db_path, 4);
        let file = codebase_path.join("lib.rs");
        std::fs::write(&file, "pub fn lib_fn() {}").unwrap();

        let stores = create_test_stores(&db_path, 4).await;
        let ids = {
            let mut vs = stores.vector_store.write().await;
            let chunk = |path: &str| {
                EmbeddedChunk::new(
                    Chunk::new(
                        "fn f() {}".to_string(),
                        0,
                        1,
                        ChunkKind::Function,
                        path.to_string(),
                    ),
                    vec![1.0, 0.0, 0.0, 0.0],
                )
            };
            vs.insert_chunks_with_ids(vec![chunk("lib.rs"), chunk("new.rs")])
                .await
                .unwrap()
        };
        // ids[1] stands for a chunk a concurrent refresh inserted but whose
        // file meta it has not saved yet.
        let mut file_meta = FileMetaStore::new("test-model".to_string(), 4);
        file_meta
            .update_file(&file, "lib.rs", vec![ids[0]])
            .unwrap();

        let changed = super::super::key_migration::repair_legacy_index_shared(
            &codebase_path,
            &mut file_meta,
            &stores,
        )
        .await
        .unwrap();

        assert!(!changed);
        assert!(
            stores
                .vector_store
                .read()
                .await
                .get_chunk(ids[1])
                .await
                .unwrap()
                .is_some(),
            "an untracked chunk must survive when no legacy key was migrated"
        );
    }

    #[tokio::test]
    async fn test_incremental_refresh_migrates_legacy_absolute_keys() {
        use crate::chunker::{Chunk, ChunkKind};
        use crate::embed::EmbeddedChunk;

        let temp = tempdir().unwrap();
        let codebase_path = temp.path().join("codebase");
        let db_path = temp.path().join("db");
        std::fs::create_dir_all(&codebase_path).unwrap();
        std::fs::create_dir_all(&db_path).unwrap();
        create_metadata_json(&db_path, 4);

        let file = codebase_path.join("lib.rs");
        std::fs::write(&file, "pub fn lib_fn() {}").unwrap();
        let absolute = file.to_string_lossy().to_string();

        let stores = create_test_stores(&db_path, 4).await;
        let chunk = |path: &str| {
            EmbeddedChunk::new(
                Chunk::new(
                    "pub fn lib_fn() {}".to_string(),
                    0,
                    1,
                    ChunkKind::Function,
                    path.to_string(),
                ),
                vec![1.0, 0.0, 0.0, 0.0],
            )
        };
        let (legacy_id, duplicate_id) = {
            let mut vs = stores.vector_store.write().await;
            let ids = vs
                .insert_chunks_with_ids(vec![chunk(&absolute), chunk("lib.rs")])
                .await
                .unwrap();
            vs.build_index().await.unwrap();
            (ids[0], ids[1])
        };

        // Pre-2c13826 layout: the file is tracked under its absolute path.
        let mut file_meta = FileMetaStore::new("test-model".to_string(), 4);
        file_meta
            .update_file(&file, &absolute, vec![legacy_id])
            .unwrap();
        file_meta.save(&db_path).unwrap();

        IndexManager::perform_incremental_refresh_with_stores(
            &codebase_path,
            &db_path,
            &stores,
            &CancellationToken::new(),
            None,
            None,
        )
        .await
        .expect("refresh over a legacy index must succeed");

        let reloaded = FileMetaStore::load_or_create(&db_path, "test-model", 4).unwrap();
        assert!(reloaded.is_tracked("lib.rs"));
        assert!(!reloaded.is_tracked(&absolute));
        assert_eq!(
            reloaded.tracked_chunk_ids(),
            std::collections::HashSet::from([legacy_id]),
            "an unchanged file must keep its chunks, not be re-embedded"
        );

        let vs = stores.vector_store.read().await;
        assert_eq!(
            vs.get_chunk(legacy_id).await.unwrap().unwrap().path,
            "lib.rs"
        );
        assert!(
            vs.get_chunk(duplicate_id).await.unwrap().is_none(),
            "the untracked duplicate chunk must be swept"
        );
    }

    /// The branch-change full rebuild must queue behind the shared guard so it
    /// cannot overlap the incremental or restore-input rebuilds on one LMDB db.
    #[tokio::test]
    async fn branch_change_symbol_rebuild_waits_for_shared_guard() {
        let temp = tempdir().unwrap();
        let guard = Arc::new(std::sync::Mutex::new(()));
        let held = guard.lock().unwrap();

        let handle = IndexManager::spawn_branch_change_symbol_rebuild(
            Arc::new(SymbolIndexerRegistry::new()),
            temp.path().to_path_buf(),
            temp.path().join("db"),
            "test".to_string(),
            None,
            None,
            CancellationToken::new(),
            guard.clone(),
        );

        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            !handle.is_finished(),
            "rebuild must block while the shared guard is held"
        );

        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(10), handle)
            .await
            .expect("rebuild must finish once the guard is released")
            .expect("rebuild task must not panic");

        assert!(
            guard.try_lock().is_ok(),
            "guard must be released after the rebuild"
        );
    }

    /// The crash-safe rollback: a failed publish must remove the fresh ids
    /// from BOTH stores so no later pass inherits orphans it cannot find
    /// (they never reach file_meta).
    #[tokio::test]
    async fn rollback_vector_insert_removes_fresh_ids_from_both_stores() {
        use crate::chunker::{Chunk, ChunkKind};
        use crate::embed::EmbeddedChunk;

        let temp = tempdir().unwrap();
        let db_path = temp.path().join("db");
        std::fs::create_dir_all(&db_path).unwrap();
        let stores = create_test_stores(&db_path, 4).await;

        let chunk = |path: &str| {
            EmbeddedChunk::new(
                Chunk::new(
                    "fn f() {}".to_string(),
                    0,
                    1,
                    ChunkKind::Function,
                    path.to_string(),
                ),
                vec![1.0, 0.0, 0.0, 0.0],
            )
        };
        let ids = {
            let mut vs = stores.vector_store.write().await;
            vs.insert_chunks_with_ids(vec![chunk("lib.rs"), chunk("util.rs")])
                .await
                .unwrap()
        };

        IndexManager::rollback_vector_insert(&stores, &ids).await;

        let remaining = {
            let vs = stores.vector_store.read().await;
            vs.get_chunks_by_file().await.unwrap()
        };
        assert!(
            remaining.is_empty(),
            "rolled-back ids must be gone from the vector store, got: {remaining:?}"
        );
    }

    #[tokio::test]
    async fn file_publication_retry_replaces_fts_documents_before_tracking_ids() {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join("db");
        std::fs::create_dir_all(&db_path).unwrap();
        let stores = create_test_stores(&db_path, 2).await;
        let file = temp.path().join("lib.rs");
        let chunks = vec![crate::embed::EmbeddedChunk::new(
            crate::chunker::Chunk::new(
                "gadget".into(),
                1,
                1,
                crate::chunker::ChunkKind::Function,
                "lib.rs".into(),
            ),
            vec![1.0, 0.0],
        )];
        let ids = stores
            .vector_store
            .write()
            .await
            .insert_chunks_with_ids(chunks.clone())
            .await
            .unwrap();
        let result = IndexManager::publish_file_chunks(
            &db_path,
            &file,
            &stores,
            "lib.rs",
            &chunks,
            &ids,
            "test-model",
            2,
        )
        .await;
        assert!(
            result.is_err(),
            "source disappeared before metadata publication"
        );
        assert_eq!(
            stores
                .fts_store
                .read()
                .await
                .search("gadget", 10, None)
                .unwrap()
                .len(),
            1
        );
        std::fs::write(&file, "gadget").unwrap();
        IndexManager::publish_file_chunks(
            &db_path,
            &file,
            &stores,
            "lib.rs",
            &chunks,
            &ids,
            "test-model",
            2,
        )
        .await
        .unwrap();
        assert_eq!(
            stores
                .fts_store
                .read()
                .await
                .search("gadget", 10, None)
                .unwrap()
                .len(),
            1
        );
        let metadata = FileMetaStore::load_or_create(&db_path, "test-model", 2).unwrap();
        assert_eq!(metadata.file_chunk_ids("lib.rs"), Some(ids));
    }

    /// Phase 3.6: vector chunks not tracked by file_meta are deleted on the
    /// next refresh, and a tracked live file whose ids are missing from the
    /// vector store is queued for re-index (heals mixed generations without
    /// a force reindex).
    #[tokio::test]
    async fn refresh_heals_mixed_generations_on_live_files() {
        use crate::chunker::{Chunk, ChunkKind};
        use crate::embed::EmbeddedChunk;

        let temp = tempdir().unwrap();
        let codebase_path = temp.path().join("codebase");
        let db_path = temp.path().join("db");
        std::fs::create_dir_all(&codebase_path).unwrap();
        std::fs::create_dir_all(&db_path).unwrap();
        create_metadata_json(&db_path, 4);

        let file = codebase_path.join("lib.rs");
        std::fs::write(&file, "pub fn lib_fn() {}").unwrap();

        let stores = create_test_stores(&db_path, 4).await;
        let chunk = |path: &str| {
            EmbeddedChunk::new(
                Chunk::new(
                    "fn f() {}".to_string(),
                    0,
                    1,
                    ChunkKind::Function,
                    path.to_string(),
                ),
                vec![1.0, 0.0, 0.0, 0.0],
            )
        };
        // ids[0] = the tracked generation; ids[1] = an orphan a failed FTS
        // commit left behind (its file exists, so Phase 3.5's ghost sweep
        // must NOT claim it — Phase 3.6's id-level check owns it).
        let ids = {
            let mut vs = stores.vector_store.write().await;
            vs.insert_chunks_with_ids(vec![chunk("lib.rs"), chunk("lib.rs")])
                .await
                .unwrap()
        };
        let mut file_meta = FileMetaStore::new("test-model".to_string(), 4);
        file_meta
            .update_file(&file, "lib.rs", vec![ids[0]])
            .unwrap();
        file_meta.save(&db_path).unwrap();

        // Simulate the failed commit's delete-phase: the tracked generation
        // is gone from the vector store, the orphan remains.
        {
            let mut vs = stores.vector_store.write().await;
            vs.delete_chunks(&[ids[0]]).await.unwrap();
        }

        let result = IndexManager::refresh_index_with_stores(
            &codebase_path,
            &db_path,
            &stores,
            &CancellationToken::new(),
            None,
        )
        .await;
        assert!(result.is_ok(), "refresh must succeed: {result:?}");

        let remaining = {
            let vs = stores.vector_store.read().await;
            vs.get_chunks_by_file().await.unwrap()
        };
        let lib_ids = remaining.get("lib.rs").cloned().unwrap_or_default();
        assert!(
            !lib_ids.contains(&ids[1]),
            "the untracked orphan must be deleted by the sweep, got: {lib_ids:?}"
        );
        // The tracked file was queued for re-index; without a real embedding
        // model the re-index attempt fails gracefully (warn + continue), so
        // the meta ids stay missing — the assertion pins the graceful path,
        // the real heal is covered by the remediation reindex after deploy.
        assert!(
            !lib_ids.contains(&ids[0]),
            "no embedding model in tests: the re-index attempt must fail gracefully, got: {lib_ids:?}"
        );
    }
}
