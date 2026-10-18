//! Always-answering status snapshot for serve mode.
//!
//! GET /status and the MCP `status` tool used to run their probes inline:
//! per registered repo several filesystem stats, a SCIP LMDB env open on
//! cache miss, and a sysinfo refresh — all synchronous on async workers.
//! Under a heavy rebuild wave those workers are OS-starved by the dotnet
//! and scip-csharp children, and one "lightweight" call took tens of
//! seconds across 28 repos; every polling client piled on and timed out.
//!
//! The fix is architectural: only a background task measures; handlers
//! read a precomputed [`StatusSnapshot`] out of a lock-held-only-to-swap
//! slot and answer in microseconds, no matter how loaded the machine is.
//! `snapshot_age_secs` in every answer tells the caller exactly how stale
//! what it is reading is — degraded freshness is visible, never a hang.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use super::{RepoStatusInfo, ServeState};

/// One computed status pass. Cheap to clone (handlers get an `Arc`).
pub(crate) struct StatusSnapshot {
    pub(crate) repos: Vec<(String, RepoStatusInfo)>,
    /// Precomputed per-repo project-list payload (metadata reads, store
    /// stats via try_read, lock probes) — list_projects answers from this
    /// for the same reason /status does: the request path never measures.
    pub(crate) projects: Vec<crate::mcp::types::RepoInfo>,
    /// Warnings captured while computing `projects` (per-repo stats
    /// failures). Re-emitted verbatim on each projects answer so a repo
    /// that failed to report stays visible.
    pub(crate) projects_warnings: Vec<String>,
    pub(crate) active_sessions: u64,
    pub(crate) cpu_percent: String,
    pub(crate) csharp_helper: bool,
    pub(crate) ts_helper: bool,
    pub(crate) generated_at: std::time::Instant,
}

impl StatusSnapshot {
    /// Age of this snapshot in whole seconds.
    pub(crate) fn age_secs(&self) -> u64 {
        self.generated_at.elapsed().as_secs()
    }
}

/// The slot handlers read from. The inner RwLock is only ever held to swap
/// an `Arc` — never across computation — so reader latency is bound by the
/// swap itself, not by how long the machine takes to measure.
#[derive(Default)]
pub(crate) struct StatusSnapshotHandle {
    slot: RwLock<Option<Arc<StatusSnapshot>>>,
}

impl StatusSnapshotHandle {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The latest snapshot, or `None` before the first background pass
    /// completes. Handlers must answer something immediately in that case,
    /// never fall back to measuring inline.
    pub(crate) fn get(&self) -> Option<Arc<StatusSnapshot>> {
        self.slot.read().ok().and_then(|s| s.clone())
    }

    pub(crate) fn store(&self, snapshot: StatusSnapshot) {
        if let Ok(mut slot) = self.slot.write() {
            *slot = Some(Arc::new(snapshot));
        }
    }
}

/// Refresh cadence: `CODESEARCH_STATUS_REFRESH_SECS` when a positive
/// integer, otherwise 2s. Unparseable values fall back to the default.
pub(crate) fn refresh_interval() -> Duration {
    match std::env::var(crate::constants::STATUS_REFRESH_SECS_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(secs) if secs > 0 => Duration::from_secs(secs),
        _ => Duration::from_secs(2),
    }
}

/// Spawn the background refresher. One pass runs at a time: the sleep
/// starts only after the previous pass completed, so under heavy load the
/// cadence stretches (visible as a growing `snapshot_age_secs`) instead of
/// piling measuring tasks onto a starved machine. The computation runs on
/// the blocking pool — it is filesystem work, not async work.
pub(crate) fn spawn(state: Arc<ServeState>) {
    let handle = state.status_snapshot.clone();
    let interval = refresh_interval();
    tokio::spawn(async move {
        loop {
            let snap = tokio::task::spawn_blocking({
                let state = state.clone();
                move || state.compute_status_snapshot()
            })
            .await;
            match snap {
                Ok(snapshot) => handle.store(snapshot),
                Err(err) => {
                    // Join failure means the runtime is shutting down; the
                    // next loop iteration would also fail, but sleeping
                    // keeps shutdown semantics simple.
                    tracing::debug!("status snapshot pass failed: {err}");
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}
