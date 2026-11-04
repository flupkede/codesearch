//! Process-wide limits for heavy indexing work.
//!
//! `codesearch serve` hosts many repos, and each repo's refresh/reindex pass
//! is CPU- and memory-heavy: file reads, tree-sitter chunking, ONNX
//! embedding, HNSW graph construction. Without a process-wide budget, N repos
//! with work start N concurrent passes — every core is saturated, and N
//! file/chunk windows are held in memory at once even though ONNX inference
//! itself already serialises on the per-model mutex.
//!
//! This module provides one shared budget ([`JobGate`]) plus the per-inference
//! and per-batch knobs that bound what a single job costs:
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `CODESEARCH_INDEX_JOBS` | `1` | Max concurrent heavy repo jobs per process |
//! | `CODESEARCH_EMBED_THREADS` | unset (ONNX default = all cores) | ONNX intra-op threads per session |
//! | `CODESEARCH_EMBED_PAUSE_MS` | `0` | Pause between background embedding mini-batches |
//! | `CODESEARCH_MIN_FREE_MB` | `0` (off) | Free-memory floor before a job may start |
//! | `CODESEARCH_MAX_CHUNKS_PER_BATCH` | `0` (off) | Hard cap on chunks held per refresh batch |
//!
//! All values are read from the environment so a systemd unit or shell can set
//! them without a config file. See the README section "Indexing limits" for
//! recommended values on shared machines.
//!
//! The gate is attached to [`crate::index::SharedStores`] at open time in
//! serve mode; standalone processes (CLI index, `codesearch mcp`) leave it
//! `None` and behave exactly as before.

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

/// Max concurrent heavy repo jobs in this process (>= 1).
pub const INDEX_JOBS_ENV: &str = "CODESEARCH_INDEX_JOBS";
/// ONNX intra-op thread count per embedding session (>= 1; unset = all cores).
pub const EMBED_THREADS_ENV: &str = "CODESEARCH_EMBED_THREADS";
/// Milliseconds to pause between background embedding mini-batches.
pub const EMBED_PAUSE_MS_ENV: &str = "CODESEARCH_EMBED_PAUSE_MS";
/// Free-memory floor (MB) before a heavy job may start (0 = disabled).
pub const MIN_FREE_MB_ENV: &str = "CODESEARCH_MIN_FREE_MB";
/// Hard cap on chunks held in memory per refresh batch (0 = disabled).
pub const MAX_CHUNKS_PER_BATCH_ENV: &str = "CODESEARCH_MAX_CHUNKS_PER_BATCH";

/// Default for [`index_jobs`]: strictly one heavy repo job at a time.
///
/// Embedding is already serialised by the per-model mutex, so running another
/// repo's refresh concurrently buys little throughput while multiplying peak
/// memory (each queued job holds its own chunk window) and store-lock
/// contention. Raise it deliberately, with `CODESEARCH_EMBED_THREADS` set, on
/// machines with cores and memory to spare.
pub const DEFAULT_INDEX_JOBS: usize = 1;

const DEFAULT_EMBED_PAUSE_MS: u64 = 0;
const DEFAULT_MIN_FREE_MB: u64 = 0;
const DEFAULT_MAX_CHUNKS_PER_BATCH: usize = 0;

/// How often the memory admission check re-reads available memory.
const MEMORY_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// How long a job waits for memory before proceeding with a warning. A
/// permanent wait would be a deadlock (the running job's ONNX arena does not
/// return memory to the OS), so this is deliberately bounded.
const MEMORY_WAIT_MAX: Duration = Duration::from_secs(30);
/// Log when a job waited at least this long for a free slot.
const SLOW_ACQUIRE_LOG_AFTER: Duration = Duration::from_secs(2);

fn env_parsed<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name)
        .ok()
        .and_then(|s| s.trim().parse::<T>().ok())
}

/// Max concurrent heavy repo jobs. Values below 1 fall back to the default.
pub fn index_jobs() -> usize {
    env_parsed::<usize>(INDEX_JOBS_ENV)
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT_INDEX_JOBS)
}

/// ONNX intra-op threads per embedding session.
///
/// `None` (unset, or a value below 1) leaves fastembed/ONNX Runtime at its
/// default, which uses every available core. Set a value to cap one inference
/// on a shared machine.
pub fn embed_threads() -> Option<usize> {
    env_parsed::<usize>(EMBED_THREADS_ENV).filter(|&n| n >= 1)
}

/// Duty-cycle pause between background embedding mini-batches.
pub fn embed_pause() -> Duration {
    Duration::from_millis(env_parsed::<u64>(EMBED_PAUSE_MS_ENV).unwrap_or(DEFAULT_EMBED_PAUSE_MS))
}

/// Free-memory floor in MB for starting a heavy job (0 = disabled).
pub fn min_free_mb() -> u64 {
    env_parsed::<u64>(MIN_FREE_MB_ENV).unwrap_or(DEFAULT_MIN_FREE_MB)
}

/// Hard cap on chunks held in memory per refresh batch (0 = disabled).
pub fn max_chunks_per_batch() -> usize {
    env_parsed::<usize>(MAX_CHUNKS_PER_BATCH_ENV).unwrap_or(DEFAULT_MAX_CHUNKS_PER_BATCH)
}

/// Available (reclaimable) system memory in MB, read fresh on each call.
pub fn available_memory_mb() -> u64 {
    let mut sys = match sysinfo::System::new() {
        Ok(s) => s,
        Err(e) => {
            // Unmeasurable must not block jobs behind the memory floor: the
            // wait loop has its own deadline and starts anyway.
            tracing::debug!("sysinfo unavailable ({e}); skipping the free-memory floor");
            return u64::MAX;
        }
    };
    sys.refresh_memory();
    sys.available_memory() / (1024 * 1024)
}

/// Bounds how many heavy repo jobs run concurrently in one process.
///
/// A "heavy job" is a whole refresh / reindex / vector-index build pass, not a
/// single inference or a query. Query embeddings never take this gate, so
/// interactive search stays responsive while a background job runs.
#[derive(Debug)]
pub struct JobGate {
    permits: Arc<Semaphore>,
    index_jobs: usize,
    min_free_mb: u64,
    memory_wait: Duration,
}

impl JobGate {
    /// Build a gate from the environment (the normal serve path).
    pub fn from_env() -> Arc<Self> {
        Self::with_config(index_jobs(), min_free_mb())
    }

    /// Build a gate with an explicit job count and memory floor.
    pub fn with_config(index_jobs: usize, min_free_mb: u64) -> Arc<Self> {
        Self::with_config_and_wait(index_jobs, min_free_mb, MEMORY_WAIT_MAX)
    }

    /// Build a gate with a custom memory wait ceiling. Test seam: production
    /// callers use [`Self::from_env`].
    pub fn with_config_and_wait(
        index_jobs: usize,
        min_free_mb: u64,
        memory_wait: Duration,
    ) -> Arc<Self> {
        let index_jobs = index_jobs.max(1);
        Arc::new(Self {
            permits: Arc::new(Semaphore::new(index_jobs)),
            index_jobs,
            min_free_mb,
            memory_wait,
        })
    }

    /// Number of concurrent job slots this gate allows.
    pub fn index_jobs(&self) -> usize {
        self.index_jobs
    }

    /// Acquire one job slot, then (when enabled) wait, bounded, for the free
    /// memory floor. The caller holds the returned permit for the whole job.
    ///
    /// Prefer [`Self::acquire_or_cancel`] for jobs that carry a cancellation
    /// token; this variant waits unbounded because there is nothing to cancel
    /// it with.
    pub async fn acquire(&self) -> JobPermit {
        self.acquire_or_cancel(&CancellationToken::new())
            .await
            .expect("a never-cancelled token can never abort the acquire")
    }

    /// Acquire one job slot, aborting promptly when `cancel` fires while
    /// queued.
    ///
    /// Returns `None` without consuming a slot when the token was already
    /// cancelled or fires before a slot is granted. A job cancelled while
    /// queued must abort rather than park: parking kept a removed or cancelled
    /// job's `Arc<SharedStores>` (and its LMDB env) alive for the permit
    /// holder's whole run, leaving "locked by another process" / double-open
    /// failures behind with nothing left to release them.
    pub async fn acquire_or_cancel(&self, cancel: &CancellationToken) -> Option<JobPermit> {
        let wait_start = Instant::now();
        if cancel.is_cancelled() {
            return None;
        }
        let permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => return None,
            permit = self.permits.clone().acquire_owned() => {
                permit.expect("job gate semaphore is never closed")
            }
        };
        let waited = wait_start.elapsed();
        if waited >= SLOW_ACQUIRE_LOG_AFTER {
            tracing::info!(
                "⏳ Indexing job waited {:.1}s for a free slot ({} slot(s) total; CODESEARCH_INDEX_JOBS)",
                waited.as_secs_f64(),
                self.index_jobs
            );
        }
        if self.min_free_mb > 0 {
            self.wait_for_memory().await;
        }
        // The memory wait above is bounded; a cancellation that lands during
        // it still wins — release the slot rather than start cancelled work.
        if cancel.is_cancelled() {
            return None;
        }
        Some(JobPermit { _permit: permit })
    }

    async fn wait_for_memory(&self) {
        let free = available_memory_mb();
        if free >= self.min_free_mb {
            return;
        }
        tracing::info!(
            "⏳ Free memory is {} MB, below the {} MB floor (CODESEARCH_MIN_FREE_MB); \
             waiting up to {:.0}s before starting the job",
            free,
            self.min_free_mb,
            self.memory_wait.as_secs_f64()
        );
        let deadline = Instant::now() + self.memory_wait;
        loop {
            tokio::time::sleep(MEMORY_POLL_INTERVAL).await;
            let free = available_memory_mb();
            if free >= self.min_free_mb {
                return;
            }
            if Instant::now() >= deadline {
                tracing::warn!(
                    "⚠️  Free memory still below CODESEARCH_MIN_FREE_MB \
                     ({} MB available, {} MB required); starting the job anyway",
                    free,
                    self.min_free_mb
                );
                return;
            }
        }
    }
}

/// Held for the duration of one heavy job; dropping it releases the slot.
#[derive(Debug)]
pub struct JobPermit {
    _permit: OwnedSemaphorePermit,
}

/// Cancellation-aware acquisition outcome for a heavy job.
///
/// `#[must_use]`: dropping a [`Self::Ready`] value releases the job slot
/// immediately (the job would run ungated), and ignoring [`Self::Cancelled`]
/// means starting work that was already cancelled. Always match on it.
#[derive(Debug)]
#[must_use]
pub enum JobAcquire {
    /// The job may proceed. The permit is `None` when no gate is attached
    /// (standalone processes keep their previous unbounded behaviour).
    Ready(Option<JobPermit>),
    /// The cancellation token fired before a slot was granted; no slot was
    /// consumed and the job must abort promptly, dropping its stores.
    Cancelled,
}

#[cfg(test)]
#[path = "limits_tests.rs"]
mod tests;
