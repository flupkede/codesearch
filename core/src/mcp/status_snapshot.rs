//! Status types shared between the serve orchestrator and core MCP handlers.
//!
//! GET /status and the MCP `status` tool answer from a precomputed
//! [`StatusSnapshot`] instead of probing inline: under a heavy rebuild wave
//! inline probing starved async workers (one "lightweight" call took tens of
//! seconds across 28 repos and every polling client piled on and timed out).
//! Only a background task measures; handlers read a
//! lock-held-only-to-swap slot and answer in microseconds. `snapshot_age_secs`
//! in every answer tells the caller exactly how stale what it is reading is.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use colored::Colorize;

/// Lightweight repo status label derived from DashMap state only (no DB opens).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoStateLabel {
    Open,
    Warm,
    Readonly,
    Idle,
    Indexing,
    Error,
    NoIndex,
}

/// Status of the C# symbol index for a repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CSharpIndexStatus {
    /// No C# helper detected or no index built.
    None,
    /// Helper available and index built successfully.
    Ready,
    /// Index built, but degraded or stale: the helper survived
    /// workspace/MSBuild failures (definitions present while cross-project
    /// references may be missing), or a rebuild was abandoned because
    /// `dotnet restore` never finished (the previous index stays live).
    /// Rendered yellow (`C#⚠`) in the TUI; the failures are in the info
    /// panel and on every find_impact answer from this index.
    Partial,
    /// Index exists but had errors or is stale.
    Error,
    /// Symbol index is currently being built.
    Indexing,
}

impl RepoStateLabel {
    pub fn colored(&self) -> colored::ColoredString {
        match self {
            Self::Open => "Open".green().bold(),
            Self::Warm => "Warm".yellow(),
            Self::Readonly => "Readonly".cyan(),
            Self::Idle => "Idle".dimmed(),
            Self::Indexing => "Indexing".magenta().bold(),
            Self::Error => "Error".red().bold(),
            Self::NoIndex => "No Index".dimmed(),
        }
    }
}

/// Lightweight repo status derived from DashMap state only (no DB opens).
pub struct RepoStatusInfo {
    pub status: RepoStateLabel,
    pub changes: u64,
    pub last_tool_call: Option<String>,
    pub tool_call_count: u64,
    pub csharp_index: CSharpIndexStatus,
    pub csharp_error: Option<String>,
    pub typescript_index: CSharpIndexStatus,
}

/// One computed status pass. Cheap to clone (handlers get an `Arc`).
pub struct StatusSnapshot {
    pub repos: Vec<(String, RepoStatusInfo)>,
    /// Precomputed per-repo project-list payload (metadata reads, store
    /// stats via try_read, lock probes) — list_projects answers from this
    /// for the same reason /status does: the request path never measures.
    pub projects: Vec<crate::mcp::types::RepoInfo>,
    /// Warnings captured while computing `projects` (per-repo stats
    /// failures). Re-emitted verbatim on each projects answer so a repo
    /// that failed to report stays visible.
    pub projects_warnings: Vec<String>,
    pub active_sessions: u64,
    pub cpu_percent: String,
    pub csharp_helper: bool,
    pub ts_helper: bool,
    pub generated_at: std::time::Instant,
}

impl StatusSnapshot {
    /// Age of this snapshot in whole seconds.
    pub fn age_secs(&self) -> u64 {
        self.generated_at.elapsed().as_secs()
    }
}

/// The slot handlers read from. The inner RwLock is only ever held to swap
/// an `Arc` — never across computation — so reader latency is bound by the
/// swap itself, not by how long the machine takes to measure.
#[derive(Default)]
pub struct StatusSnapshotHandle {
    slot: RwLock<Option<Arc<StatusSnapshot>>>,
}

impl StatusSnapshotHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// The latest snapshot, or `None` before the first background pass
    /// completes. Handlers must answer something immediately in that case,
    /// never fall back to measuring inline.
    pub fn get(&self) -> Option<Arc<StatusSnapshot>> {
        self.slot.read().ok().and_then(|s| s.clone())
    }

    pub fn store(&self, snapshot: StatusSnapshot) {
        if let Ok(mut slot) = self.slot.write() {
            *slot = Some(Arc::new(snapshot));
        }
    }
}

/// Refresh cadence: `CODESEARCH_STATUS_REFRESH_SECS` when a positive
/// integer, otherwise 2s. Unparseable values fall back to the default.
pub fn refresh_interval() -> Duration {
    match std::env::var(crate::constants::STATUS_REFRESH_SECS_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        Some(secs) if secs > 0 => Duration::from_secs(secs),
        _ => Duration::from_secs(2),
    }
}
