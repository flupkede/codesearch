//! Phase 3 of serve startup: after the symbol wave, load the resident
//! scip-csharp workspace of the most recently used C# repos so the first
//! `find_impact` after a restart is warm. Loads go through the normal
//! `WorkspacePool` spawn path, so the helper gate and the per-worktree lock
//! apply; the cap is the pool's own admission limit.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::{resolve_csharp_status, CSharpIndexStatus, ServeState};
use crate::constants::{
    check_shutdown, DB_DIR_NAME, DEFAULT_SCIP_RESIDENT_PREWARM, LANG_CSHARP,
    SCIP_RESIDENT_PREWARM_ENV,
};

/// Loads one repo's resident workspace; blocks for the whole load.
pub(super) type PrewarmFn = Arc<dyn Fn(&Path) -> Result<()> + Send + Sync>;

/// Only an exact `0` turns the prewarm off.
pub(super) fn prewarm_enabled() -> bool {
    match std::env::var(SCIP_RESIDENT_PREWARM_ENV) {
        Ok(raw) => raw != "0",
        Err(_) => DEFAULT_SCIP_RESIDENT_PREWARM,
    }
}

/// The first `cap` eligible aliases, most recently used first, within the
/// warmup age gate — the same ordering and gate phase-1 warmup applies.
/// `eligible` runs lazily in that order and never past the cap.
pub(super) fn select_prewarm_aliases(
    aliases: Vec<String>,
    access: &HashMap<String, u64>,
    now_ms: u64,
    max_age_days: u64,
    cap: usize,
    mut eligible: impl FnMut(&str) -> bool,
) -> Vec<String> {
    let ordered = ServeState::warmup_order(aliases, access);
    let (fresh, _stale) = ServeState::warmup_partition(ordered, access, now_ms, max_age_days);
    fresh
        .into_iter()
        .filter(|alias| eligible(alias))
        .take(cap)
        .collect()
}

/// Load `targets` one after another; returns the aliases that loaded. A
/// failed load is logged and skipped. Stops at the next boundary once
/// `cancel` or the global shutdown flag fires; a load already running
/// finishes on its blocking thread and the pool discards it.
pub(super) async fn run_prewarm(
    targets: Vec<(String, PathBuf)>,
    cancel: &CancellationToken,
    spawn: PrewarmFn,
) -> Vec<String> {
    let mut loaded = Vec::with_capacity(targets.len());
    for (alias, path) in targets {
        if check_shutdown(cancel) {
            info!("phase-3: prewarm cancelled by shutdown");
            break;
        }
        let started = Instant::now();
        let job = {
            let spawn = Arc::clone(&spawn);
            tokio::task::spawn_blocking(move || spawn(&path))
        };
        let outcome = tokio::select! {
            _ = cancel.cancelled() => {
                info!("phase-3: prewarm cancelled by shutdown while loading '{}'", alias);
                break;
            }
            outcome = job => outcome,
        };
        match outcome {
            Ok(Ok(())) => {
                info!(
                    "phase-3: '{}' resident workspace ready in {} s",
                    alias,
                    started.elapsed().as_secs()
                );
                loaded.push(alias);
            }
            Ok(Err(e)) => warn!("phase-3: prewarm of '{}' failed: {e:#}", alias),
            Err(e) => warn!("phase-3: prewarm of '{}' panicked: {e:?}", alias),
        }
    }
    loaded
}

impl ServeState {
    /// Phase 3: prewarm the resident workspaces of the most recently used
    /// C# repos. Runs after phase 2, so a repo's rebuild has settled (and
    /// evicted any stale resident) before its workspace loads.
    pub(crate) async fn run_resident_prewarm(self: &Arc<Self>, cancel: &CancellationToken) {
        let state = Arc::clone(self);
        let spawn: PrewarmFn = Arc::new(move |path| match state.symbol_registry.get(LANG_CSHARP) {
            Some(indexer) => indexer.prewarm_resident(path),
            None => Ok(()),
        });
        let cap = crate::symbols::resident::WORKSPACE_POOL.max_resident();
        self.run_resident_prewarm_with(cancel, cap, spawn).await;
    }

    pub(super) async fn run_resident_prewarm_with(
        self: &Arc<Self>,
        cancel: &CancellationToken,
        cap: usize,
        spawn: PrewarmFn,
    ) -> Vec<String> {
        if !prewarm_enabled() {
            info!(
                "phase-3: disabled via {}=0 — resident workspaces load on first find_impact",
                SCIP_RESIDENT_PREWARM_ENV
            );
            return Vec::new();
        }
        // Applicability and the readiness probe touch the filesystem and
        // may open an LMDB env: keep them off the async workers.
        let state = Arc::clone(self);
        let targets =
            match tokio::task::spawn_blocking(move || state.select_prewarm_targets(cap)).await {
                Ok(targets) => targets,
                Err(e) => {
                    warn!("phase-3: prewarm selection panicked: {e:?}");
                    return Vec::new();
                }
            };
        if targets.is_empty() {
            info!("phase-3 complete: no C# repo to prewarm");
            return Vec::new();
        }
        info!(
            "phase-3: prewarming {} resident workspace(s): {}",
            targets.len(),
            targets
                .iter()
                .map(|(alias, _)| alias.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let loaded = run_prewarm(targets, cancel, spawn).await;
        info!("phase-3 complete: {} workspace(s) prewarmed", loaded.len());
        loaded
    }

    fn select_prewarm_targets(&self, cap: usize) -> Vec<(String, PathBuf)> {
        let access = self.load_access_times();
        let selected = select_prewarm_aliases(
            self.aliases(),
            &access,
            Self::unix_now_ms(),
            Self::warmup_max_age_days(),
            cap,
            |alias| self.csharp_prewarm_eligible(alias),
        );
        selected
            .into_iter()
            .filter_map(|alias| {
                let path = self.config.read().ok().and_then(|c| c.resolve(&alias))?;
                Some((alias, path))
            })
            .collect()
    }

    /// A repo is worth a resident workspace when the C# adapter applies to it
    /// and its symbol index is Ready: a repo with no usable index would have
    /// its first `find_impact` fall back to a one-shot run regardless.
    fn csharp_prewarm_eligible(&self, alias: &str) -> bool {
        let Some(path) = self.config.read().ok().and_then(|c| c.resolve(alias)) else {
            return false;
        };
        let Some(indexer) = self.symbol_registry.get(LANG_CSHARP) else {
            return false;
        };
        if !indexer.applies_to(&path) {
            return false;
        }
        let cached = self.csharp_index_status.get(alias).map(|e| *e.value());
        let status = resolve_csharp_status(cached, self.has_symbol_indexing_marker(alias), || {
            self.probe_csharp_index(&path.join(DB_DIR_NAME))
        });
        status == CSharpIndexStatus::Ready
    }
}

#[cfg(test)]
#[path = "prewarm_tests.rs"]
mod tests;
