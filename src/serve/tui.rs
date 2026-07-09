//! ratatui-based TUI for `codesearch serve`.
//!
//! Replaces the old `print_dashboard()` eprintln approach with a fullscreen
//! alternate-screen TUI that renders a live status table without flickering.
//!
//! Rendering and key handling are shared with the remote TUI via `tui_common`.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::Terminal;

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::{self, EnterAlternateScreen};

use tokio_util::sync::CancellationToken;

use super::tui_common::{
    self, KeyAction, OverlayKeyAction, OverlayState, RemoteIndexStats, RemoteStatsState, RepoRow,
};
use super::ServeState;
use crate::cli::doctor;
use crate::constants::{DB_DIR_NAME, LANG_CSHARP};
use crate::index::IndexManager;

/// Footer flash shown when a local-index action key (doctor / reindex / remove)
/// is pressed while a mounted remote project is selected. Those actions operate
/// on a local index, which a peer-hosted mount doesn't have — this confirms the
/// no-op the struck-through footer hint already signals.
const REMOTE_ACTION_NA: &str = "✗ doctor / reindex / remove don't apply to a remote mount";

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the fullscreen TUI.  Spawns as a tokio task from `run_serve`.
///
/// Returns `Ok(())` when the user presses `q`, or when the
/// `cancel_token` is cancelled externally (e.g. Ctrl-C from the main task).
///
/// Terminal restoration is guaranteed on normal exit and on errors.
/// Panics mid-frame are extremely unlikely (ratatui is panic-free in practice)
/// and the OS will restore raw mode on process exit as a last resort.
pub async fn run_tui(
    state: Arc<ServeState>,
    cancel_token: CancellationToken,
    serve_url: String,
) -> io::Result<()> {
    // Setup terminal
    crossterm::execute!(io::stdout(), EnterAlternateScreen)?;
    terminal::enable_raw_mode()?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // Run main loop. Errors (e.g. from terminal.draw) propagate up
    // and are caught below to ensure restoration.
    let result = run_tui_loop(&mut terminal, state, cancel_token, &serve_url).await;

    // Always restore terminal, even on error
    tui_common::restore_terminal(&mut terminal)?;

    result
}

async fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: Arc<ServeState>,
    cancel_token: CancellationToken,
    serve_url: &str,
) -> io::Result<()> {
    // TUI-local state
    let mut table_state = ratatui::widgets::TableState::default();
    table_state.select(Some(0));
    let tick_interval = Duration::from_millis(500);
    let poll_timeout = Duration::from_millis(100);

    // sysinfo System instance — must persist across frames so cpu_usage()
    // can compute a delta between refresh calls (first call always returns 0).
    let mut sys_system: Option<sysinfo::System> = None;

    // Optional modal overlay (dismissed by Esc)
    let mut overlay: Option<OverlayState> = None;

    // Transient footer flash (action confirmations like "reindex started").
    // Auto-clears after FLASH_TTL so it never sticks.
    let mut flash: Option<(String, std::time::Instant)> = None;
    const FLASH_TTL: Duration = Duration::from_secs(4);

    // Channel to receive doctor results from background task. The payload is
    // tagged with a request generation so a late result from a request the
    // user already dismissed (or superseded with a newer one) is ignored.
    let (doctor_tx, mut doctor_rx) = tokio::sync::mpsc::channel::<(u64, OverlayState)>(1);
    // Monotonic id of the most recent doctor request; bumped on every spawn.
    let mut doctor_gen: u64 = 0;

    // Mounted remote projects (peer-hosted indexes, shown italic). Discovered on
    // a slow background cadence so per-peer HTTP never blocks the render tick;
    // the latest snapshot is cached here and appended after the local rows.
    let (remote_tx, mut remote_rx) = tokio::sync::mpsc::channel::<Vec<RepoRow>>(1);
    let mut remote_rows: Vec<RepoRow> = Vec::new();
    spawn_remote_discovery(state.clone(), remote_tx, cancel_token.clone());

    // Main loop
    loop {
        // Absorb the newest remote-projects snapshot, if the background task
        // produced one since the last tick (keep the previous list otherwise).
        while let Ok(latest) = remote_rx.try_recv() {
            remote_rows = latest;
        }

        // Draw the UI — local repos first, mounted remote projects appended.
        let repos = state.repo_statuses_lightweight();
        let mut rows = map_repo_rows(&repos, &state);
        rows.extend(remote_rows.iter().cloned());

        // Clamp selection
        if !rows.is_empty() {
            let sel = table_state.selected().unwrap_or(0);
            if sel >= rows.len() {
                table_state.select(Some(rows.len() - 1));
            }
        }

        // Load session count + CPU for footer
        let active = state.active_session_count();
        let cpu = cpu_usage_str(&mut sys_system);
        let version = env!("CARGO_PKG_VERSION");
        let uptime = tui_common::format_uptime(state.started_at());
        let csharp_helper = state
            .symbol_registry
            .get(LANG_CSHARP)
            .map(|i| i.is_available())
            .unwrap_or(false);

        // Expire a stale flash, then borrow the live message (if any) for render.
        if flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= FLASH_TTL)
        {
            flash = None;
        }
        let flash_msg = flash.as_ref().map(|(msg, _)| msg.as_str());

        terminal.draw(|f| {
            let size = f.area();
            let chunks = Layout::vertical([
                Constraint::Length(3), // header
                Constraint::Min(4),    // body (table)
                Constraint::Length(4), // detail panel (selected repo info + optional error)
                Constraint::Length(1), // footer
            ])
            .split(size);

            tui_common::render_header(f, chunks[0], serve_url, version, false, &uptime);
            tui_common::render_table(f, chunks[1], &rows, &mut table_state);
            tui_common::render_detail(f, chunks[2], &rows, &table_state, 4);
            tui_common::render_footer(
                f,
                chunks[3],
                &rows,
                &table_state,
                active,
                &cpu,
                csharp_helper,
                flash_msg,
            );

            // Render overlay on top of everything if active
            if let Some(ref ov) = overlay {
                tui_common::render_overlay(f, size, ov);
            }
        })?;

        // Check if doctor result arrived from background task. Only apply it if
        // the user is still waiting on the current request (spinner showing and
        // generation matches); otherwise the result is stale — drain and drop it.
        if let Ok((gen, result)) = doctor_rx.try_recv() {
            // Apply async results (doctor diagnostics OR remote-mount index stats)
            // only if the user is still viewing the matching overlay and the
            // generation matches; otherwise the result is stale — drop it.
            if gen == doctor_gen
                && matches!(
                    overlay,
                    Some(OverlayState::DoctorRunning { .. })
                        | Some(OverlayState::RemoteInfo { .. })
                )
            {
                overlay = Some(result);
            }
        }

        // Poll for key events
        let mut should_quit = false;
        while event::poll(poll_timeout)? {
            if let Event::Key(key) = event::read()? {
                // On Windows, crossterm emits both Press and Release events.
                // Only act on Press to avoid double-stepping (scroll by 2).
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                // If overlay is active, handle overlay-specific keys
                if let Some(ref ov) = overlay {
                    if let Some(action) = tui_common::handle_overlay_key(key, ov) {
                        match action {
                            OverlayKeyAction::Dismiss => overlay = None,
                            OverlayKeyAction::ConfirmRemove => {
                                if let Some(OverlayState::ConfirmRemove { alias }) = overlay.take()
                                {
                                    let state_bg = state.clone();
                                    tokio::spawn(async move {
                                        tracing::info!(
                                            "TUI: Removing repo '{}' after confirmation",
                                            alias
                                        );
                                        let _ = state_bg.remove_repo(&alias).await;
                                    });
                                }
                            }
                        }
                    }
                    continue;
                }

                if tui_common::is_quit_key(key) {
                    should_quit = true;
                    break;
                }
                match tui_common::handle_key(key, &mut table_state, rows.len()) {
                    KeyAction::Reload => {
                        // 'l' pressed — force reload of repos config
                        // Clear mtime so reload_if_changed actually reloads
                        if let Ok(mut mtime_guard) = state.config_mtime.write() {
                            *mtime_guard = None;
                        }
                        let _ = state.reload_if_changed();
                    }
                    KeyAction::ShowInfo(idx) => {
                        if idx < repos.len() {
                            // Local repo — gather live on-disk index stats.
                            if let Some(ov) = build_info_overlay(idx, &repos, &state) {
                                overlay = Some(ov);
                            }
                        } else if let Some(row) = rows.get(idx) {
                            // Mounted remote project (appended after local rows).
                            // Show federation coordinates immediately, then fetch
                            // the peer's on-disk index stats in the background.
                            let base = build_remote_info_overlay(row);
                            // Bump the generation UNCONDITIONALLY: this invalidates
                            // any still-in-flight doctor/remote-info result (they
                            // share one channel + counter) so a late reply can't
                            // clobber this overlay via the recv guard below.
                            doctor_gen += 1;
                            if let Some(crate::db_discovery::repos::Target::RemoteProject {
                                peer,
                                remote_alias,
                                ..
                            }) = state.config_snapshot().resolve_remote_project(&row.alias)
                            {
                                overlay = Some(base.clone());
                                spawn_remote_info(
                                    base,
                                    peer,
                                    remote_alias,
                                    doctor_tx.clone(),
                                    doctor_gen,
                                );
                            } else {
                                // Mount no longer resolves (misconfig, or a config
                                // reload raced this keypress). No fetch will run, so
                                // don't leave the overlay stuck on "fetching…".
                                overlay =
                                    Some(with_remote_stats(base, RemoteStatsState::Unavailable));
                            }
                        }
                    }
                    KeyAction::RunDoctor(idx) => {
                        if idx < repos.len() {
                            let alias = repos[idx].0.clone();
                            // Show "running" overlay immediately
                            overlay = Some(OverlayState::DoctorRunning {
                                alias: alias.clone(),
                            });
                            // Spawn background task to run diagnostics, tagged
                            // with a fresh generation so its result is only
                            // applied if this request is still the current one.
                            doctor_gen += 1;
                            spawn_doctor(alias, state.clone(), doctor_tx.clone(), doctor_gen);
                        } else {
                            // Remote mount (appended after local rows) — the
                            // footer already greys this out; confirm the no-op.
                            flash = Some((REMOTE_ACTION_NA.to_string(), std::time::Instant::now()));
                        }
                    }
                    KeyAction::ForceReindex(idx) => {
                        if idx < repos.len() {
                            let alias = repos[idx].0.clone();
                            let msg = match spawn_force_reindex(alias.clone(), &state) {
                                ReindexLaunch::Started => {
                                    format!("⟳ Reindex started for '{alias}' …")
                                }
                                ReindexLaunch::AlreadyRunning => {
                                    format!("⟳ Reindex already running for '{alias}'")
                                }
                                ReindexLaunch::Failed => {
                                    format!("✗ Cannot reindex '{alias}' — see logs")
                                }
                            };
                            flash = Some((msg, std::time::Instant::now()));
                        } else {
                            flash = Some((REMOTE_ACTION_NA.to_string(), std::time::Instant::now()));
                        }
                    }
                    KeyAction::RequestRemove(idx) => {
                        if idx < repos.len() {
                            let alias = repos[idx].0.clone();
                            overlay = Some(OverlayState::ConfirmRemove { alias });
                        } else {
                            flash = Some((REMOTE_ACTION_NA.to_string(), std::time::Instant::now()));
                        }
                    }
                    KeyAction::None => {}
                }
            }
        }

        if should_quit {
            // User pressed q — signal shutdown to the whole serve process
            cancel_token.cancel();
            break;
        }

        if cancel_token.is_cancelled() {
            break;
        }

        tokio::time::sleep(tick_interval).await;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Data mapping: ServeState → RepoRow
// ---------------------------------------------------------------------------

/// Map internal `repo_statuses_lightweight()` data to the shared `RepoRow` type.
fn map_repo_rows(
    repos: &[(String, super::RepoStatusInfo)],
    state: &Arc<ServeState>,
) -> Vec<RepoRow> {
    let config = state.config_snapshot();
    repos
        .iter()
        .map(|(alias, info)| {
            let status_str = match info.status {
                super::RepoStateLabel::Open => "open",
                super::RepoStateLabel::Warm => "warm",
                super::RepoStateLabel::Readonly => "readonly",
                super::RepoStateLabel::Closed => "closed",
                super::RepoStateLabel::Indexing => "indexing",
                super::RepoStateLabel::Error => "error",
                super::RepoStateLabel::NoIndex => "no_index",
            }
            .to_string();

            let csharp_str = match info.csharp_index {
                super::CSharpIndexStatus::Ready => "ready",
                super::CSharpIndexStatus::Indexing => "indexing",
                super::CSharpIndexStatus::Error => "error",
                super::CSharpIndexStatus::None => "none",
            }
            .to_string();

            let lock_mode = match info.status {
                super::RepoStateLabel::Open | super::RepoStateLabel::Indexing => "write",
                super::RepoStateLabel::Warm | super::RepoStateLabel::Readonly => "read",
                _ => "—",
            }
            .to_string();

            let path = config
                .resolve(alias)
                .map(|p| p.display().to_string())
                .unwrap_or_default();

            RepoRow {
                alias: alias.clone(),
                status: status_str,
                csharp_index: csharp_str,
                csharp_error: info.csharp_error.clone(),
                changes: info.changes,
                tool_call_count: info.tool_call_count,
                last_tool_call: info.last_tool_call.clone(),
                lock_mode,
                path,
                is_remote: false,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Mounted remote projects (federation) — background discovery
// ---------------------------------------------------------------------------

/// Spawn the background task that periodically re-discovers mounted remote
/// projects and pushes the latest `RepoRow` list through `tx`.
///
/// Runs off the render loop so per-peer HTTP never blocks a frame. Each round
/// queries every configured peer's `/status` concurrently; peers that answer
/// refresh an in-memory "last-known" alias list, and peers that fail this round
/// reuse it — so a transient blip doesn't make a mount vanish from the table.
fn spawn_remote_discovery(
    state: Arc<ServeState>,
    tx: tokio::sync::mpsc::Sender<Vec<RepoRow>>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let client = match crate::federation::FederationClient::new() {
            Ok(c) => c,
            // No HTTP client (e.g. TLS init failure) → no remote mounts, ever.
            Err(e) => {
                tracing::warn!("remote discovery disabled: HTTP client init failed: {e}");
                return;
            }
        };
        let interval = Duration::from_secs(crate::constants::REMOTE_DISCOVERY_INTERVAL_SECS);

        loop {
            let cfg = state.config_snapshot();
            if !cfg.remotes.is_empty() {
                let rows = discover_remote_rows(&client, &cfg).await;
                // Capacity-1 channel: replace the pending snapshot if the render
                // loop hasn't consumed it yet (try_send drops on Full — fine, the
                // next round supersedes it anyway).
                let _ = tx.try_send(rows);
            }

            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(interval) => {}
            }
        }
    });
}

/// Build the mounted remote-project rows for the TUI.
///
/// Rows come from the opt-in [`remote_mounts`](crate::db_discovery::repos::ReposConfig::remote_mounts)
/// allowlist (config-driven, so they always show). Every peer's `/status` is
/// polled concurrently only to *enrich* those rows with live per-repo state;
/// a mount whose peer is unreachable this round simply falls back to a "warm"
/// default. Discovery never defines which projects are mounted.
async fn discover_remote_rows(
    client: &crate::federation::FederationClient,
    cfg: &crate::db_discovery::repos::ReposConfig,
) -> Vec<RepoRow> {
    use crate::db_discovery::repos::Target;
    use crate::federation::ManagementOutcome;

    // 1) Fan out /status to all peers concurrently, keyed (peer, remote_alias).
    let mut status_lookup: std::collections::HashMap<
        (String, String),
        crate::federation::RemoteRepoStatus,
    > = std::collections::HashMap::new();

    let mut join = tokio::task::JoinSet::new();
    for (peer_name, peer) in cfg.remotes.iter() {
        let client = client.clone();
        let peer = peer.clone();
        let peer_name = peer_name.clone();
        join.spawn(async move {
            let outcome = client.list_repos(&peer).await;
            (peer_name, outcome)
        });
    }
    while let Some(res) = join.join_next().await {
        if let Ok((peer_name, ManagementOutcome::Ok(status))) = res {
            for r in status.repos {
                status_lookup.insert((peer_name.clone(), r.alias.clone()), r);
            }
        }
    }

    // 2) Build one row per mounted project, enriched with live status.
    cfg.mounted_remote_projects()
        .into_iter()
        .map(|(local_name, target)| {
            let Target::RemoteProject {
                peer_name,
                peer,
                remote_alias,
            } = target
            else {
                // mounted_remote_projects only ever yields RemoteProject.
                unreachable!("mounted_remote_projects yielded a non-RemoteProject target");
            };
            let st = status_lookup.get(&(peer_name, remote_alias));
            RepoRow {
                alias: local_name,
                // Reuse the peer's own status vocabulary (open/warm/…); default
                // to "warm" for a cached-but-unreachable peer.
                status: st
                    .map(|s| s.status.clone())
                    .unwrap_or_else(|| "warm".to_string()),
                csharp_index: "none".to_string(),
                csharp_error: None,
                changes: st.map(|s| s.changes).unwrap_or(0),
                tool_call_count: st.and_then(|s| s.tool_call_count).unwrap_or(0),
                last_tool_call: st.and_then(|s| s.last_tool_call.clone()),
                lock_mode: st.map(|s| s.lock_mode.clone()).unwrap_or_default(),
                // Detail panel shows where the mount lives.
                path: peer.url.clone(),
                is_remote: true,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// CPU usage (embedded only — remote TUI gets it from HTTP)
// ---------------------------------------------------------------------------

/// Get current process CPU usage as a human-readable string.
///
/// Uses `sysinfo` crate — fully cross-platform, no platform-specific code.
///
/// **Important:** `sys_system` must be reused across calls so `cpu_usage()` can
/// compute a delta between refresh calls (first call always returns 0%).
fn cpu_usage_str(sys_system: &mut Option<sysinfo::System>) -> String {
    use sysinfo::{ProcessesToUpdate, System};

    let pid = match sysinfo::get_current_pid() {
        Ok(p) => p,
        Err(_) => return "—".into(),
    };

    // Create System instance on first call, reuse on subsequent calls.
    let sys = sys_system.get_or_insert_with(|| {
        let mut s = System::new();
        s.refresh_cpu_list(sysinfo::CpuRefreshKind::nothing());
        s
    });

    // Refresh only our process (cpu)
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);

    match sys.process(pid) {
        Some(proc) => {
            let num_cpus = sys.cpus().len().max(1) as f32;
            let pct = proc.cpu_usage() / num_cpus;
            format!("{:.0}%", pct)
        }
        None => "—".into(),
    }
}

// ---------------------------------------------------------------------------
// Info overlay builder
// ---------------------------------------------------------------------------

/// Build an `OverlayState::Info` by gathering live stats from SharedStores or metadata.
fn build_info_overlay(
    idx: usize,
    repos: &[(String, super::RepoStatusInfo)],
    state: &Arc<ServeState>,
) -> Option<OverlayState> {
    if idx >= repos.len() {
        return None;
    }
    let (alias, _info) = &repos[idx];
    let config = state.config_snapshot();
    let project_path = config.resolve(alias)?;
    let db_path = project_path.join(DB_DIR_NAME);

    // Try to get live stats from opened stores
    let mut chunks = 0usize;
    let mut files = 0usize;
    let mut max_chunk_id = 0u32;
    let mut dims = 0usize;
    let mut model = String::from("unknown");
    let mut lock = String::from("—");
    let mut index_age = String::from("—");

    // Read model + dims from metadata.json
    if let Ok(content) = std::fs::read_to_string(db_path.join("metadata.json")) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            model = json
                .get("model_short_name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            dims = json.get("dimensions").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

            // Total chunks from metadata (may be 0 if clobbered — Stage 2 fix)
            chunks = json
                .get("total_chunks")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            files = json
                .get("total_files")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;

            // Index age from indexed_at
            if let Some(indexed_at) = json.get("indexed_at").and_then(|v| v.as_str()) {
                index_age = format_age(indexed_at);
            }
        }
    }

    // If stores are open, get live stats (overrides metadata)
    if let Some(stores) = state.get_opened_stores(alias) {
        if let Ok(vs) = stores.vector_store.try_read() {
            if let Ok(live_stats) = vs.stats() {
                chunks = live_stats.total_chunks;
                files = live_stats.total_files;
                max_chunk_id = live_stats.max_chunk_id;
                if dims == 0 {
                    dims = live_stats.dimensions;
                }
            }
        }
        lock = if stores.readonly {
            "read".to_string()
        } else {
            "write".to_string()
        };
    }

    // DB size on disk
    let db_size_human = dir_size_human(&db_path);

    Some(OverlayState::Info {
        alias: alias.clone(),
        chunks,
        files,
        max_chunk_id,
        db_size_human,
        model,
        dims,
        lock,
        index_age,
    })
}

/// Build a `RemoteInfo` overlay for a mounted remote project row.
///
/// Remote mounts have no local index on disk (chunks / db size / model live on
/// the peer), so this surfaces the federation coordinates (peer URL) plus the
/// peer-reported live status already carried on the `RepoRow`.
fn build_remote_info_overlay(row: &RepoRow) -> OverlayState {
    OverlayState::RemoteInfo {
        alias: row.alias.clone(),
        peer_url: row.path.clone(),
        status: row.status.clone(),
        lock: if row.lock_mode.is_empty() {
            "—".to_string()
        } else {
            row.lock_mode.clone()
        },
        changes: row.changes,
        tool_call_count: row.tool_call_count,
        last_tool_call: row.last_tool_call.clone(),
        // Index stats (chunks/files/db-size/model) live on the peer and are
        // fetched asynchronously; start in the loading state.
        stats: RemoteStatsState::Loading,
    }
}

/// Format an ISO 8601 timestamp as a human-readable age string.
pub(crate) fn format_age(iso_ts: &str) -> String {
    let parsed = chrono::DateTime::parse_from_rfc3339(iso_ts).or_else(|_| {
        // Try without timezone (assume UTC)
        chrono::NaiveDateTime::parse_from_str(iso_ts, "%Y-%m-%dT%H:%M:%S%.f")
            .map(|dt| dt.and_utc().fixed_offset())
    });

    match parsed {
        Ok(dt) => {
            let now = chrono::Utc::now();
            let dur = now.signed_duration_since(dt);
            if dur.num_seconds() < 0 {
                return "just now".to_string();
            }
            let mins = dur.num_minutes();
            if mins < 1 {
                "just now".to_string()
            } else if mins < 60 {
                format!("{}m ago", mins)
            } else {
                let hours = mins / 60;
                if hours < 24 {
                    format!("{}h ago", hours)
                } else {
                    let days = hours / 24;
                    format!("{}d ago", days)
                }
            }
        }
        Err(_) => iso_ts.to_string(),
    }
}

/// Compute total size of a directory on disk, formatted as human-readable string.
pub(crate) fn dir_size_human(path: &std::path::Path) -> String {
    let total_bytes = walkdir_size(path);
    if total_bytes == 0 {
        return "—".to_string();
    }
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if total_bytes >= GB {
        format!("{:.1} GB", total_bytes as f64 / GB as f64)
    } else if total_bytes >= MB {
        format!("{:.1} MB", total_bytes as f64 / MB as f64)
    } else {
        format!("{:.0} KB", total_bytes as f64 / KB as f64)
    }
}

/// Walk a directory and sum file sizes. Returns 0 on any error.
fn walkdir_size(path: &std::path::Path) -> u64 {
    let mut total: u64 = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if let Ok(file_type) = entry.file_type() {
                if file_type.is_file() {
                    if let Ok(meta) = entry.metadata() {
                        total += meta.len();
                    }
                } else if file_type.is_dir() {
                    total += walkdir_size(&entry.path());
                }
            }
        }
    }
    total
}

// ---------------------------------------------------------------------------
// Doctor (non-blocking spawn)
// ---------------------------------------------------------------------------

/// Spawn a background task to run doctor diagnostics for the given repo alias.
/// Sends the result overlay back via `tx` when done, tagged with `gen` so the
/// receiver can discard results from dismissed or superseded requests.
fn spawn_doctor(
    alias: String,
    state: Arc<ServeState>,
    tx: tokio::sync::mpsc::Sender<(u64, OverlayState)>,
    gen: u64,
) {
    let resolved = state.config.read().ok().and_then(|c| c.resolve(&alias));
    let project_path = match resolved {
        Some(p) => p,
        None => {
            let _ = tx.try_send((
                gen,
                OverlayState::Doctor {
                    alias,
                    results: vec![
                        "✗ Cannot resolve alias to path".to_string(),
                        String::new(),
                        "  [Esc] close".to_string(),
                    ],
                },
            ));
            return;
        }
    };

    tokio::spawn(async move {
        let result_overlay = async {
            let stores = state.get_or_open_stores(&alias, true).await;
            match stores {
                Ok(s) => {
                    let vs = s.vector_store.read().await;
                    let report = doctor::diagnose_with_store(&project_path, &vs);
                    drop(vs);
                    match report {
                        Ok(r) => OverlayState::Doctor {
                            alias,
                            results: r.render_tui(),
                        },
                        Err(e) => OverlayState::Doctor {
                            alias,
                            results: vec![
                                format!("✗ Doctor failed: {}", e),
                                String::new(),
                                "  [Esc] close".to_string(),
                            ],
                        },
                    }
                }
                Err(e) => OverlayState::Doctor {
                    alias,
                    results: vec![
                        format!("✗ Cannot open database: {}", e),
                        String::new(),
                        "  [Esc] close".to_string(),
                    ],
                },
            }
        }
        .await;

        // Send result back to TUI loop (non-blocking — if channel closed, just drop it)
        let _ = tx.send((gen, result_overlay)).await;
    });
}

/// Spawn a background task to fetch a mounted remote project's on-disk index
/// stats from the peer's `GET /repos/{alias}/info`, then send an enriched
/// `RemoteInfo` overlay back via `tx` tagged with `gen` (so a stale or dismissed
/// request's result is discarded by the receiver). On any failure the overlay
/// falls back to `RemoteStatsState::Unavailable` — the mount coordinates already
/// shown remain intact.
fn spawn_remote_info(
    base: OverlayState,
    peer: crate::db_discovery::repos::RemotePeer,
    remote_alias: String,
    tx: tokio::sync::mpsc::Sender<(u64, OverlayState)>,
    gen: u64,
) {
    tokio::spawn(async move {
        use crate::federation::{FederationClient, ManagementOutcome};
        let stats = match FederationClient::new() {
            Ok(client) => match client.repo_info(&peer, &remote_alias).await {
                ManagementOutcome::Ok(info) => RemoteStatsState::Ready(RemoteIndexStats {
                    chunks: info.chunks,
                    files: info.files,
                    db_size_human: info.db_size_human,
                    model: info.model,
                }),
                // Peer unreachable / non-2xx / unparseable — surface as unavailable.
                _ => RemoteStatsState::Unavailable,
            },
            // No HTTP client (e.g. TLS init failure) → stats can't be fetched.
            Err(_) => RemoteStatsState::Unavailable,
        };
        let enriched = with_remote_stats(base, stats);
        let _ = tx.send((gen, enriched)).await;
    });
}

/// Replace the `stats` field of a `RemoteInfo` overlay, leaving any other overlay
/// variant untouched.
fn with_remote_stats(overlay: OverlayState, stats: RemoteStatsState) -> OverlayState {
    if let OverlayState::RemoteInfo {
        alias,
        peer_url,
        status,
        lock,
        changes,
        tool_call_count,
        last_tool_call,
        ..
    } = overlay
    {
        OverlayState::RemoteInfo {
            alias,
            peer_url,
            status,
            lock,
            changes,
            tool_call_count,
            last_tool_call,
            stats,
        }
    } else {
        overlay
    }
}

// ---------------------------------------------------------------------------
// Force reindex (non-blocking spawn)
// ---------------------------------------------------------------------------

/// Outcome of a TUI force-reindex launch — used to drive immediate footer
/// feedback. Only describes whether the background task *started*; the actual
/// indexing result is reported later via the status column / logs.
enum ReindexLaunch {
    /// Background reindex task spawned successfully.
    Started,
    /// A reindex was already running for this alias — request ignored.
    AlreadyRunning,
    /// Could not start (config error, unresolved alias, read-only, open error).
    Failed,
}

/// Spawn a background force reindex task for the given repo alias.
/// Follows the same flow as the HTTP `reindex_handler`.
fn spawn_force_reindex(alias: String, state: &Arc<ServeState>) -> ReindexLaunch {
    // Guard against concurrent reindex
    if !state.begin_indexing(&alias) {
        tracing::warn!(
            "Force reindex already in progress for '{}', skipping TUI request",
            alias
        );
        return ReindexLaunch::AlreadyRunning;
    }

    let config = match state.config.read() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Config lock poisoned: {}", e);
            state.end_indexing(&alias);
            return ReindexLaunch::Failed;
        }
    };
    let project_path = match config.resolve(&alias) {
        Some(p) => p,
        None => {
            tracing::error!("Cannot resolve alias '{}' for force reindex", alias);
            state.end_indexing(&alias);
            return ReindexLaunch::Failed;
        }
    };
    drop(config); // release read lock

    let db_path = project_path.join(DB_DIR_NAME);

    // Stop FSW
    let stores = match state.stop_fsw(&alias) {
        Some(s) => s,
        None => {
            // Try to open stores (allow_create=true for recovery)
            let cancel = CancellationToken::new();
            match state.try_open_stores(&alias, &db_path, true) {
                Ok(super::OpenedStores::Write(s)) => {
                    state.repos.insert(
                        alias.clone(),
                        super::RepoState::Write {
                            stores: s.clone(),
                            index_manager: None,
                            cancel_token: cancel,
                        },
                    );
                    state.touch_access(&alias);
                    s
                }
                Ok(super::OpenedStores::Readonly(_)) => {
                    tracing::error!(
                        "Repo {} opened read-only; cannot force-reindex from TUI",
                        alias
                    );
                    state.end_indexing(&alias);
                    return ReindexLaunch::Failed;
                }
                Err(e) => {
                    tracing::error!("Cannot open stores for '{}': {}", alias, e);
                    state.end_indexing(&alias);
                    return ReindexLaunch::Failed;
                }
            }
        }
    };

    let alias_bg = alias.clone();
    let state_bg = state.clone();
    tokio::spawn(async move {
        tracing::info!(
            "TUI: Force reindex for '{}': clearing stores and reindexing",
            alias_bg
        );

        match IndexManager::force_reindex_with_stores(&project_path, &db_path, &stores, None).await
        {
            Ok(()) => {
                tracing::info!("TUI: Force reindex complete for '{}'", alias_bg);
            }
            Err(e) => {
                tracing::error!("TUI: Force reindex failed for '{}': {}", alias_bg, e);
            }
        }

        // Restart FSW with fresh IndexManager
        state_bg.restart_fsw(&alias_bg, stores).await;

        // Remove guard
        state_bg.end_indexing(&alias_bg);
    });

    ReindexLaunch::Started
}

// ---------------------------------------------------------------------------
// TTY detection
// ---------------------------------------------------------------------------

/// Check if stdout is connected to a real terminal (TTY).
/// Returns `false` when piped, redirected, or running as a service.
pub fn is_tty() -> bool {
    // crossterm::terminal::size() returns Err when stdout is not a real terminal.
    crossterm::terminal::size().is_ok()
}

/// Attempt to start the TUI. Returns None if no TTY is available.
/// Logs a one-line message to stderr in non-TTY mode.
pub fn maybe_spawn_tui(
    state: Arc<ServeState>,
    cancel_token: CancellationToken,
    serve_url: String,
) -> Option<tokio::task::JoinHandle<()>> {
    if !is_tty() {
        return None;
    }
    Some(tokio::spawn(async move {
        if let Err(e) = run_tui(state, cancel_token, serve_url).await {
            tracing::error!("TUI error: {}", e);
        }
    }))
}
