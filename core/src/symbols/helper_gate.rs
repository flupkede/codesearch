//! Process-wide blocking gate bounding heavy scip-csharp work (index runs,
//! one-shot find-refs, resident spawn + initial workspace load): one job per
//! worktree root, and `CSHARP_SCIP_CONCURRENCY` jobs across all worktrees.
//! Blocking by design: every caller already runs on a blocking thread.
//! Always the innermost locks — serve's phase-2 semaphore and the resident
//! pool's per-solution spawn lock are taken first; within the gate the
//! worktree lock precedes the global slot, so a job queued behind its own
//! worktree never holds a slot another worktree could use.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Condvar, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

use crate::cache::{normalize_path, normalize_user_path, safe_canonicalize};
use crate::constants::{
    csharp_scip_concurrency, is_shutdown_requested, CSHARP_HELPER_GATE_SLOW_WAIT_MS,
};

static GLOBAL: LazyLock<Arc<HelperGate>> =
    LazyLock::new(|| Arc::new(HelperGate::new(csharp_scip_concurrency())));

/// The process-wide gate; capacity read once from `CSHARP_SCIP_CONCURRENCY`.
pub(crate) fn global() -> &'static Arc<HelperGate> {
    &GLOBAL
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HelperPurpose {
    Rebuild,
    FindRefs,
    ResidentSpawn,
}

impl std::fmt::Display for HelperPurpose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Rebuild => "rebuild",
            Self::FindRefs => "find-refs",
            Self::ResidentSpawn => "resident-spawn",
        })
    }
}

pub(crate) struct HelperGate {
    capacity: usize,
    in_use: Mutex<usize>,
    freed: Condvar,
    /// Keyed by normalized worktree root. Never removed (bounded by repo
    /// count, like the resident pool's spawn locks).
    worktrees: Mutex<HashMap<String, Arc<WorktreeLock>>>,
}

impl HelperGate {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            in_use: Mutex::new(0),
            freed: Condvar::new(),
            worktrees: Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    /// Block until `worktree_root` has no other heavy job running, then
    /// until a global slot is free.
    pub(crate) fn acquire(
        &self,
        purpose: HelperPurpose,
        worktree_root: &Path,
    ) -> Result<HelperPermit<'_>> {
        let started = Instant::now();
        let (worktree, worktree_waited) = self.worktree_lock(worktree_root).lock();
        // A lock or slot freed by serve's shutdown kill must not start a
        // helper that kill can no longer reach; the guards dropped on bail
        // pass the wakeup on so other waiters bail too.
        if worktree_waited && is_shutdown_requested() {
            return Err(cancelled(purpose, worktree_root));
        }

        let mut in_use = self.in_use.lock().unwrap_or_else(PoisonError::into_inner);
        let waited = *in_use >= self.capacity;
        while *in_use >= self.capacity {
            in_use = self
                .freed
                .wait(in_use)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if waited && is_shutdown_requested() {
            drop(in_use);
            self.freed.notify_one();
            return Err(cancelled(purpose, worktree_root));
        }
        *in_use += 1;
        drop(in_use);

        let elapsed = started.elapsed();
        if elapsed >= Duration::from_millis(CSHARP_HELPER_GATE_SLOW_WAIT_MS) {
            tracing::info!(
                "scip-csharp helper gate: {purpose} for {} waited {} ms for its worktree and a slot (capacity {})",
                worktree_root.display(),
                elapsed.as_millis(),
                self.capacity
            );
        }
        Ok(HelperPermit {
            gate: self,
            _worktree: worktree,
        })
    }

    fn worktree_lock(&self, worktree_root: &Path) -> Arc<WorktreeLock> {
        let canonical =
            safe_canonicalize(worktree_root).unwrap_or_else(|_| normalize_user_path(worktree_root));
        let key = normalize_path(&canonical);
        let mut locks = self
            .worktrees
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Arc::clone(locks.entry(key).or_default())
    }

    fn release(&self) {
        let mut in_use = self.in_use.lock().unwrap_or_else(PoisonError::into_inner);
        *in_use = in_use.saturating_sub(1);
        drop(in_use);
        self.freed.notify_one();
    }
}

fn cancelled(purpose: HelperPurpose, worktree_root: &Path) -> anyhow::Error {
    anyhow!(
        "scip-csharp {purpose} for {} cancelled: shutdown requested",
        worktree_root.display()
    )
}

/// A per-worktree exclusive lock whose guard owns its `Arc`, so the guard
/// can live inside a [`HelperPermit`] (a std `MutexGuard` borrows its mutex).
#[derive(Default)]
struct WorktreeLock {
    held: Mutex<bool>,
    freed: Condvar,
}

impl WorktreeLock {
    /// Returns the guard and whether the caller had to wait for it.
    fn lock(self: Arc<Self>) -> (WorktreeGuard, bool) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let waited = *held;
        while *held {
            held = self
                .freed
                .wait(held)
                .unwrap_or_else(PoisonError::into_inner);
        }
        *held = true;
        drop(held);
        (WorktreeGuard(self), waited)
    }
}

struct WorktreeGuard(Arc<WorktreeLock>);

impl Drop for WorktreeGuard {
    fn drop(&mut self) {
        let lock = &self.0;
        *lock.held.lock().unwrap_or_else(PoisonError::into_inner) = false;
        lock.freed.notify_one();
    }
}

/// The worktree lock plus one global slot. Drop releases the slot first
/// (in `drop`), then the worktree lock (the field, dropped after `drop`).
#[must_use = "dropping the permit releases the slot immediately"]
pub(crate) struct HelperPermit<'a> {
    gate: &'a HelperGate,
    _worktree: WorktreeGuard,
}

impl Drop for HelperPermit<'_> {
    fn drop(&mut self) {
        self.gate.release();
    }
}
