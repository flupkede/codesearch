//! Background continuation for budget-overrun `find_impact` lookups.
//!
//! When the `find_impact` wall-clock budget overruns, the answering handler
//! returns a structured busy envelope while the lookup keeps running as a
//! detached blocking task (its reference-cache writes still land in LMDB).
//! This module is the tracking half of that design: a registry keyed by
//! (project, symbol identity) so a retry observes current progress, or the
//! warm precise result once the detached lookup finished, instead of
//! racing a second identical helper subprocess against a cold cache.
//!
//! Semantics:
//! - `register` get-or-creates the entry: two racing first-callers share
//!   one entry, so the second is answered from it (dedupe) rather than
//!   starting a duplicate subprocess.
//! - A `Running` entry is reported with cumulative elapsed time.
//! - A `Finished` entry is consumed on read (removed from the map): exactly
//!   one retry sees the warm result; later lookups go through the normal
//!   path and hit the real reference cache, which is warm by then.
//! - Entries expire after a TTL (see `FIND_IMPACT_TRACK_TTL_SECS`), which
//!   also covers a blocking task that died without recording (a panic in
//!   the helper call): the next lookup after expiry starts fresh.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::constants::FIND_IMPACT_TRACK_TTL_SECS;
use crate::symbols::SymbolReference;

/// Identity of a tracked lookup: (project db dir, symbol identity string).
///
/// The identity string is the same rendering the busy envelope shows —
/// `'Ns.I.M'` for name lookups, `file:line` for position lookups. Two
/// requests that spell the same symbol differently (e.g. absolute vs
/// relative file paths on the position variant) map to different keys;
/// that only weakens dedupe, never correctness.
pub(crate) type LookupKey = (PathBuf, String);

/// Recorded outcome of a finished lookup. The error is the rendered
/// `{:#}` chain (`anyhow::Error` is not `Clone`); the typed failure
/// envelope is built from this at response time.
pub(crate) type RecordedResult = Result<Vec<SymbolReference>, String>;

enum EntryState {
    Running {
        started: Instant,
    },
    Finished {
        result: RecordedResult,
        finished: Instant,
    },
}

/// One tracked lookup. Shared between the answering handler and the
/// detached blocking task via `Arc`; `finish` is called from INSIDE the
/// blocking task so the outcome is recorded even when the handler's
/// awaiting future was dropped at budget overrun.
pub(crate) struct LookupEntry {
    state: Mutex<EntryState>,
}

impl LookupEntry {
    fn new() -> Self {
        Self {
            state: Mutex::new(EntryState::Running {
                started: Instant::now(),
            }),
        }
    }

    /// Record the lookup outcome. A panic inside the blocked call never
    /// reaches this — such an entry stays `Running` until the TTL drops it.
    pub(crate) fn finish(&self, result: RecordedResult) {
        let mut state = self.lock();
        *state = EntryState::Finished {
            result,
            finished: Instant::now(),
        };
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, EntryState> {
        // Poisoning can only come from a panic between lock and assignment,
        // which is no state worth preserving: recover the inner guard.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// What `ImpactLookupTracker::check` reports about a tracked lookup.
#[derive(Debug)]
pub(crate) enum TrackedStatus {
    /// Still running; `elapsed_ms` is cumulative wall-clock since start.
    Running { elapsed_ms: u64 },
    /// Finished — `Ok` is the warm precise result, `Err` the rendered
    /// failure chain. Consumed on read.
    Done(RecordedResult),
}

/// Registry of in-flight / recently-finished `find_impact` lookups.
pub(crate) struct ImpactLookupTracker {
    ttl: Duration,
    entries: Mutex<HashMap<LookupKey, Arc<LookupEntry>>>,
}

impl ImpactLookupTracker {
    pub(crate) fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Observe a tracked lookup: progress while running, the outcome once
    /// finished (consumed), or `None` when nothing is tracked (expired or
    /// never started) so the caller starts a fresh lookup.
    pub(crate) fn check(&self, key: &LookupKey) -> Option<TrackedStatus> {
        // Clone the entry out under a short map borrow; the state read and
        // the consume-removal happen without holding the map lock.
        let entry = {
            let mut map = self.lock();
            self.sweep(&mut map);
            map.get(key).cloned()?
        };
        let state = entry.lock();
        match &*state {
            EntryState::Running { started } => Some(TrackedStatus::Running {
                elapsed_ms: elapsed_millis(started.elapsed()),
            }),
            EntryState::Finished { result, .. } => {
                // Consume: the served retry is the one the busy advice
                // addressed; everyone after it hits the warm reference
                // cache through the normal path.
                let result = result.clone();
                drop(state);
                self.lock().remove(key);
                Some(TrackedStatus::Done(result))
            }
        }
    }

    /// Get-or-create the entry for `key`. Racing first-callers share one
    /// entry — both lookups record the same outcome into it, and whichever
    /// finishes first answers (or the entry is consumed by a retry).
    pub(crate) fn register(&self, key: LookupKey) -> Arc<LookupEntry> {
        let mut map = self.lock();
        self.sweep(&mut map);
        map.entry(key)
            .or_insert_with(|| Arc::new(LookupEntry::new()))
            .clone()
    }

    /// Drop the entry after a lookup that finished WITHIN the budget: it
    /// completed synchronously, nothing is in flight, and a later lookup
    /// must consult the real cache rather than a remembered result.
    pub(crate) fn remove(&self, key: &LookupKey) {
        self.lock().remove(key);
    }

    fn sweep(&self, map: &mut HashMap<LookupKey, Arc<LookupEntry>>) {
        map.retain(|_, entry| match &*entry.lock() {
            EntryState::Running { started } => started.elapsed() < self.ttl,
            EntryState::Finished { finished, .. } => finished.elapsed() < self.ttl,
        });
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<LookupKey, Arc<LookupEntry>>> {
        self.entries.lock().unwrap_or_else(|p| p.into_inner())
    }
}

fn elapsed_millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Process-global tracker. Keys include the project db dir, so one global
/// serves every project without threading state through the service
/// constructors; the TTL bounds growth, and entries live only for the
/// duration of one lookup plus one retry.
pub(crate) static IMPACT_LOOKUP_TRACKER: LazyLock<ImpactLookupTracker> =
    LazyLock::new(|| ImpactLookupTracker::new(Duration::from_secs(FIND_IMPACT_TRACK_TTL_SECS)));

#[cfg(test)]
#[path = "find_impact_tracker_tests.rs"]
mod find_impact_tracker_tests;
