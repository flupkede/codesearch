//! ratatui-based TUI for `codesearch serve`.
//!
//! Replaces the old `print_dashboard()` eprintln approach with a fullscreen
//! alternate-screen TUI that renders a live status table without flickering.
//!
//! Rendering and key handling are shared with the remote TUI via `tui_common`.

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::Terminal;

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::{self, EnterAlternateScreen};

use tokio_util::sync::CancellationToken;

use super::tui_common::{
    self, KeyAction, OverlayKeyAction, OverlayState, RemoteIndexStats, RemoteStatsState, RepoRow,
};
use super::{IndexTask, IndexingOwner, ServeState};
use crate::cli::doctor;
use crate::constants::{DB_DIR_NAME, LANG_CSHARP, LANG_TYPESCRIPT};
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
    let session_probe = Arc::clone(&state);
    let result = run_tui_loop(&mut terminal, state, cancel_token, &serve_url).await;

    // Always restore terminal, even on error
    tui_common::restore_terminal(&mut terminal)?;

    // After `q` the serve keeps draining open MCP sessions for up to ~3 s before
    // force-exiting — from the terminal this pause looks like a hang, so say so.
    let user_quit = result.as_ref().is_ok_and(|quit| *quit);
    if user_quit {
        println!(
            "{}",
            shutdown_drain_notice(session_probe.active_session_count())
        );
    }

    result.map(|_| ())
}

/// The post-`q` shutdown notice. Extracted so its contract is pinned by a
/// test: name the bounded wait (~3 s), count the draining sessions, and say
/// the process exits by itself — the notice exists precisely to stop the
/// reflex second Ctrl-C that hard-kills serve mid-drain once raw mode is off.
pub(crate) fn shutdown_drain_notice(active_sessions: u64) -> String {
    if active_sessions == 0 {
        "🛑 Shutting down…".to_string()
    } else {
        format!(
            "🛑 Shutting down: waiting up to ~3 s for {active_sessions} open MCP session(s) to \
             drain, then exiting by itself — no Ctrl-C needed."
        )
    }
}

async fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: Arc<ServeState>,
    cancel_token: CancellationToken,
    serve_url: &str,
) -> io::Result<bool> {
    // TUI-local state
    let mut table_state = ratatui::widgets::TableState::default();
    table_state.select(Some(0));
    let mut user_quit = false;
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

    // Collapsed group keys (family main alias / peer name), toggled with
    // space. Stale keys after a config reload are harmless: they match no
    // anchor and hide nothing.
    let mut collapsed: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Channel to receive doctor results from background task. The payload is
    // tagged with a request generation so a late result from a request the
    // user already dismissed (or superseded with a newer one) is ignored.
    let (doctor_tx, mut doctor_rx) = tokio::sync::mpsc::channel::<(u64, OverlayState)>(1);
    // Monotonic id of the most recent doctor request; bumped on every spawn.
    let mut doctor_gen: u64 = 0;
    // Generation of the in-flight deferred info request, if any. The local
    // info overlay is built off the UI loop (its file/meta reads blocked
    // every arrow-key press); the result applies only when its generation
    // still matches, so a superseded request can never clobber a newer one.
    let mut pending_info_gen: Option<u64> = None;

    // Mounted remote projects (peer-hosted indexes, shown italic). The background
    // task rebuilds these rows from config alone and NEVER polls a peer on a
    // timer; the only refresh trigger is a poke sent the moment a real tool call
    // hits a peer (so a scale-to-zero peer is never woken by the dashboard). The
    // latest snapshot is cached here and appended after the local rows.
    let (remote_tx, mut remote_rx) = tokio::sync::mpsc::channel::<RemoteDiscoveryUpdate>(1);
    // Poke channel: the render loop sends a peer name here when it detects that
    // peer's activity advanced (a real tool call), triggering an immediate
    // single-peer `/status` refresh in the discovery task.
    let (poke_tx, poke_rx) = tokio::sync::mpsc::channel::<String>(8);
    let mut remote_rows: Vec<RepoRow> = Vec::new();
    // Per-peer wall-clock of the last successful `/status` refresh (reported by
    // the discovery task). A row whose peer hasn't been refreshed within
    // `REMOTE_ACTIVITY_FRESH_SECS` renders its activity as a stale `-`.
    let mut remote_refreshed_at: std::collections::HashMap<String, std::time::Instant> =
        std::collections::HashMap::new();
    // High-water mark of each peer's last real activity (from
    // `ServeState::remote_peer_last_activity`). An advance vs. the previous tick
    // means a tool call just used that peer → poke an immediate refresh.
    let mut peer_activity_hwm: std::collections::HashMap<String, std::time::Instant> =
        std::collections::HashMap::new();
    spawn_remote_discovery(state.clone(), remote_tx, poke_rx, cancel_token.clone());

    // Main loop
    loop {
        // Absorb the newest remote-projects snapshot, if the background task
        // produced one since the last tick (keep the previous list otherwise).
        while let Ok(update) = remote_rx.try_recv() {
            remote_rows = update.rows;
            for (peer, t) in update.refreshed_at {
                remote_refreshed_at.insert(peer, t);
            }
        }

        // Federation-only housekeeping (local repos are untouched): for each
        // mounted remote project, (a) mark its activity stale/fresh from its
        // peer's last refresh time, and (b) detect peers whose real activity
        // advanced since the last tick and poke the discovery task to refresh
        // just them. A peer with no mounts contributes nothing and is never
        // polled — the activity poke is the *only* thing that ever contacts a
        // peer from here, so a federated peer can stay scaled to zero for as
        // long as nobody actually queries it.
        let fresh_window = Duration::from_secs(crate::constants::REMOTE_ACTIVITY_FRESH_SECS);
        let mut alias_to_peer: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut peers_to_poke: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (local_name, target) in state.config_snapshot().mounted_remote_projects() {
            if let crate::db_discovery::repos::Target::RemoteProject { peer_name, .. } = target {
                alias_to_peer.insert(local_name, peer_name);
            }
        }
        for row in remote_rows.iter_mut() {
            let Some(peer) = alias_to_peer.get(&row.alias) else {
                continue;
            };
            // (a) staleness: fresh only if refreshed within the window.
            let fresh = remote_refreshed_at
                .get(peer)
                .is_some_and(|t| t.elapsed() < fresh_window);
            row.activity_stale = !fresh;
            // (b) activity advance → poke an immediate per-peer refresh.
            if let Some(activity) = state.remote_peer_last_activity(peer) {
                match peer_activity_hwm.get(peer).copied() {
                    None => {
                        // Seed: don't poke for activity that predates the TUI.
                        peer_activity_hwm.insert(peer.clone(), activity);
                    }
                    Some(prev) if activity > prev => {
                        peers_to_poke.insert(peer.clone());
                        peer_activity_hwm.insert(peer.clone(), activity);
                    }
                    _ => {}
                }
            }
        }
        for peer in peers_to_poke {
            // try_send on a capacity-8 channel; a dropped poke just means the
            // discovery task already has a refresh queued for this peer.
            let _ = poke_tx.try_send(peer);
        }

        // Draw the UI — local repos first, mounted remote projects appended.
        let repos = state.repo_statuses_lightweight();
        let mut rows = map_repo_rows(&repos, &state);
        rows.extend(remote_rows.iter().cloned());

        // Peer mounts group visually under a synthetic peer header row
        // (e.g. `cloud` above `cloud/akeneo`): every mount of the peer gets
        // the `| ` group indent so no repo reads as another's parent.
        group_peer_mounts(&mut rows);
        // Space-collapsed groups hide their child rows; anchors stay visible.
        let rows = visible_rows(&rows, &collapsed);

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

        let ts_helper = state
            .symbol_registry
            .get(LANG_TYPESCRIPT)
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

        // Body height of the frame about to be drawn — the page step for
        // PageUp/PageDown (data rows = body minus the table header chrome).
        let mut body_height: u16 = 0;
        terminal.draw(|f| {
            let size = f.area();
            let chunks = Layout::vertical([
                Constraint::Length(3), // header
                Constraint::Min(4),    // body (table)
                Constraint::Length(4), // detail panel (selected repo info + optional error)
                Constraint::Length(1), // footer
            ])
            .split(size);
            body_height = chunks[1].height;

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
                ts_helper,
                flash_msg,
            );

            // Render overlay on top of everything if active
            if let Some(ref ov) = overlay {
                tui_common::render_overlay(f, size, ov);
            }
        })?;

        // Page step for this frame's body height (0 when the body is all
        // chrome — page_target floors it at one row).
        let page_size = body_height.saturating_sub(tui_common::TABLE_HEADER_ROWS) as usize;

        // Check if a background result arrived (deferred info OR doctor OR
        // remote-mount index stats — they share one channel + counter). Only
        // apply it if the generation still matches the current request;
        // otherwise the result is stale — drain and drop it.
        if let Ok((gen, result)) = doctor_rx.try_recv() {
            if gen == doctor_gen {
                if pending_info_gen == Some(gen) {
                    // Deferred local info: there is no placeholder overlay to
                    // guard — the overlay simply appears when the build is
                    // done (the issue allows "pas na korte delay").
                    pending_info_gen = None;
                    overlay = Some(result);
                } else if matches!(
                    overlay,
                    Some(OverlayState::DoctorRunning { .. })
                        | Some(OverlayState::RemoteInfo { .. })
                ) {
                    overlay = Some(result);
                }
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
                            OverlayKeyAction::CopyWarnings => {
                                if let Some(ov) = overlay.as_mut() {
                                    tui_common::apply_copy_warnings(ov);
                                }
                            }
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
                match tui_common::handle_key(key, &mut table_state, rows.len(), page_size) {
                    KeyAction::Reload => {
                        // 'l' pressed — force reload of repos config
                        // Clear mtime so reload_if_changed actually reloads
                        if let Ok(mut mtime_guard) = state.config_mtime.write() {
                            *mtime_guard = None;
                        }
                        let _ = state.reload_if_changed();
                    }
                    KeyAction::ShowInfo(idx) => {
                        // The table renders the REORDERED rows — resolve the
                        // action against rows[idx], never repos[idx]: the
                        // snapshot's order diverges once worktree families are
                        // grouped (selection-index mismatch → wrong repo).
                        match rows.get(idx) {
                            // Local repo — build the info overlay in the
                            // background (its file/meta reads are the
                            // arrow-key lag this deferral removes); the
                            // overlay appears when the result arrives.
                            Some(row) if !row.is_remote => {
                                doctor_gen += 1;
                                pending_info_gen = Some(doctor_gen);
                                spawn_info_overlay(
                                    row.alias.clone(),
                                    state.clone(),
                                    doctor_tx.clone(),
                                    doctor_gen,
                                );
                            }
                            // Mounted remote project (appended after local rows).
                            // Show federation coordinates immediately, then fetch
                            // the peer's on-disk index stats in the background.
                            Some(row) => {
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
                                    overlay = Some(with_remote_stats(
                                        base,
                                        RemoteStatsState::Unavailable,
                                    ));
                                }
                            }
                            None => {}
                        }
                    }
                    KeyAction::RunDoctor(idx) => match rows.get(idx) {
                        Some(row) if !row.is_remote => {
                            let alias = row.alias.clone();
                            // Show "running" overlay immediately
                            overlay = Some(OverlayState::DoctorRunning {
                                alias: alias.clone(),
                            });
                            // Spawn background task to run diagnostics, tagged
                            // with a fresh generation so its result is only
                            // applied if this request is still the current one.
                            doctor_gen += 1;
                            spawn_doctor(alias, state.clone(), doctor_tx.clone(), doctor_gen);
                        }
                        _ => {
                            // Remote mount (appended after local rows) — the
                            // footer already greys this out; confirm the no-op.
                            flash = Some((REMOTE_ACTION_NA.to_string(), std::time::Instant::now()));
                        }
                    },
                    KeyAction::ForceReindex(idx) => match rows.get(idx) {
                        Some(row) if !row.is_remote => {
                            let alias = row.alias.clone();
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
                        }
                        _ => {
                            flash = Some((REMOTE_ACTION_NA.to_string(), std::time::Instant::now()));
                        }
                    },
                    KeyAction::RequestRemove(idx) => match rows.get(idx) {
                        Some(row) if !row.is_remote => {
                            overlay = Some(OverlayState::ConfirmRemove {
                                alias: row.alias.clone(),
                            });
                        }
                        _ => {
                            flash = Some((REMOTE_ACTION_NA.to_string(), std::time::Instant::now()));
                        }
                    },
                    KeyAction::ToggleCollapse(idx) => {
                        if let Some(key) = toggle_group_key(&rows, idx) {
                            // Toggle: present → expand (remove), absent → collapse.
                            if !collapsed.remove(&key) {
                                collapsed.insert(key);
                            }
                        }
                    }
                    KeyAction::None => {}
                }
            }
        }

        if should_quit {
            // User pressed q — signal shutdown to the whole serve process
            user_quit = true;
            cancel_token.cancel();
            break;
        }

        if cancel_token.is_cancelled() {
            break;
        }

        tokio::time::sleep(tick_interval).await;
    }

    Ok(user_quit)
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
    // Grouped rendering (human-facing only): each family renders as a tree —
    // the main-tree row followed directly by its worktrees, each indented
    // with `| `; the Worktree column carries only the main's "+ N worktrees"
    // count (the tree shape carries the relationship). Counting scope: the
    // whole fleet (the TUI lists every registered repo).
    let family_map = config.worktree_family_map();
    let mut family_sizes: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for main in family_map.values() {
        *family_sizes.entry(main.clone()).or_insert(0) += 1;
    }
    let mut by_main: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    for (wt, main) in &family_map {
        by_main.entry(main.as_str()).or_default().push(wt.as_str());
    }
    let is_worktree: std::collections::HashSet<&str> =
        family_map.keys().map(String::as_str).collect();

    // Reorder: families anchor at the main's own position; a worktree met
    // before its main is skipped there and emitted under the main instead.
    // Worktrees whose main is missing from this snapshot (transient between
    // config and status) keep their row, still indented.
    let mut ordered: Vec<(&(String, super::RepoStatusInfo), bool)> = Vec::new();
    let mut emitted: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for entry in repos.iter() {
        if is_worktree.contains(entry.0.as_str()) {
            continue;
        }
        ordered.push((entry, false));
        if let Some(wts) = by_main.get(entry.0.as_str()) {
            let mut wts: Vec<&str> = wts.clone();
            wts.sort_unstable();
            for wt in wts {
                if let Some(w_entry) = repos.iter().find(|(a, _)| a == wt) {
                    ordered.push((w_entry, true));
                }
                emitted.insert(wt);
            }
        }
    }
    for entry in repos.iter() {
        if is_worktree.contains(entry.0.as_str()) && !emitted.contains(entry.0.as_str()) {
            ordered.push((entry, true));
        }
    }

    ordered
        .into_iter()
        .map(|((alias, info), is_wt)| {
            let status_str = match info.status {
                super::RepoStateLabel::Open => "open",
                super::RepoStateLabel::Warm => "warm",
                super::RepoStateLabel::Readonly => "readonly",
                super::RepoStateLabel::Idle => "idle",
                super::RepoStateLabel::Indexing => "indexing",
                super::RepoStateLabel::Error => "error",
                super::RepoStateLabel::NoIndex => "no_index",
            }
            .to_string();

            let csharp_str = match info.csharp_index {
                super::CSharpIndexStatus::Ready => "ready",
                super::CSharpIndexStatus::Partial => "partial",
                super::CSharpIndexStatus::Indexing => "indexing",
                super::CSharpIndexStatus::Error => "error",
                super::CSharpIndexStatus::None => "none",
            }
            .to_string();

            let ts_str = match info.typescript_index {
                super::CSharpIndexStatus::Ready => "ready",
                super::CSharpIndexStatus::Partial => "partial",
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
                display_name: None,
                status: status_str,
                csharp_index: csharp_str,
                csharp_error: info.csharp_error.clone(),
                typescript_index: ts_str,
                changes: info.changes,
                tool_call_count: info.tool_call_count,
                last_tool_call: info.last_tool_call.clone(),
                lock_mode,
                path,
                is_remote: false,
                // Local repos carry live serve state — never stale.
                activity_stale: false,
                // Worktree rows show the relationship via the `| ` tree
                // indent, so the note column stays count-only.
                family_note: if is_wt {
                    None
                } else {
                    crate::db_discovery::repos::family_note(&family_map, &family_sizes, alias)
                },
                indent: is_wt,
                group_header: false,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Mounted remote projects (federation) — background discovery
// ---------------------------------------------------------------------------

/// One background-discovery snapshot pushed from [`spawn_remote_discovery`] to
/// the render loop: the rebuilt remote rows plus the per-peer wall-clock of the
/// last successful `/status` refresh (used to mark cached activity stale).
struct RemoteDiscoveryUpdate {
    rows: Vec<RepoRow>,
    refreshed_at: std::collections::HashMap<String, std::time::Instant>,
}

/// Query one peer's `/status`, returning its repo list on success or `None` if
/// the peer is unreachable / errored this round (caller keeps the cached row).
async fn poll_peer_status(
    client: &crate::federation::FederationClient,
    peer: &crate::db_discovery::repos::RemotePeer,
) -> Option<Vec<crate::federation::RemoteRepoStatus>> {
    use crate::federation::ManagementOutcome;
    match client.list_repos(peer).await {
        ManagementOutcome::Ok(status) => Some(status.repos),
        _ => None,
    }
}

/// Spawn the background task that discovers mounted remote projects and pushes
/// snapshots through `tx`.
///
/// **Scale-to-zero design: a federated peer is NEVER polled on a timer.** Local
/// repos are refreshed freely by the render loop; a *federated* peer is
/// contacted only when there is a real reason to:
///
/// - an **activity poke** — a genuine federated tool call just hit that peer, so
///   it is already awake and refreshing it costs nothing. The render loop detects
///   the advance via [`ServeState::remote_peer_last_activity`] and sends the peer
///   name on `poke_rx`; only that peer is refreshed, never a full fan-out, so an
///   idle sibling peer is not touched.
/// - an explicit operator keypress — `i` on a remote row fetches that peer's
///   index stats for the info overlay (see `spawn_remote_info`). That is a
///   deliberate human action, not background traffic. `l` (reload) only re-reads
///   the local config and contacts nobody.
///
/// The periodic tick below is **config-only**
/// ([`crate::constants::REMOTE_ROW_REFRESH_SECS`]): it rebuilds the rows from the
/// `remote_mounts` allowlist so mount/unmount edits and `l` reloads appear
/// promptly, and issues no HTTP whatsoever. Outside of active use a mount
/// therefore renders its activity as a stale `-` and a scale-to-zero peer stays
/// asleep indefinitely. A peer that blips on a poke keeps its cached row (a mount
/// never vanishes on a transient failure).
///
/// **Why there is no baseline poll.** An earlier version ran a `/status` fan-out
/// on the *local* serve's idle-suspend window (2h by default), on the theory that
/// polling no faster than the suspend term was harmless. It is not: each poll
/// *woke* a sleeping replica, which then held itself warm for its own full idle
/// window (1h on the cloud deploy) — a ~50% duty cycle on a peer nobody queried.
/// Not keeping a peer awake past its suspend term is not the same as not waking
/// it, and the two windows were unrelated values besides (local vs. peer).
/// Group remote peer mounts visually under their peer prefix: the first
/// mount per peer stays unindented as the anchor, the rest get the `| `
/// indent. Local rows and remotes without a `peer/alias` shape are skipped.
/// Group peer mounts under a synthetic header row carrying the federated
/// peer's name and its server hostname (`<remote-name>/<hostname>`, parsed
/// from the mount's peer URL; a URL without a host falls back to the remote
/// name alone): `cloud/akeneo` and `cloud/erp` render as
/// `| akeneo` / `| erp` under a `cloud/host.example` header — the remote
/// name ("cloud") is a local mount label, not part of the repo name. Header
/// rows are remote-shaped (empty cells, unindented); every mount of the peer is
/// indented and displays its bare repo name (`display_name`; `alias` stays
/// the full query name). Single-mount peers and bare remotes (no `/`) stay
/// as-is — the misleading-parentage problem only exists with two or more
/// mounts. Locals are never touched.
fn group_peer_mounts(rows: &mut Vec<RepoRow>) {
    fn hostname_of(url: &str, fallback: &str) -> String {
        let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
        // IPv6 literals: the bracketed address IS the host — a plain split on
        // ':' would truncate to '['.
        let host = if after_scheme.starts_with('[') {
            let end = after_scheme
                .find(']')
                .map(|i| i + 1)
                .unwrap_or(after_scheme.len());
            &after_scheme[..end]
        } else {
            after_scheme.split([':', '/']).next().unwrap_or_default()
        };
        let host = host.trim();
        if host.is_empty() {
            fallback.to_string()
        } else {
            host.to_string()
        }
    }
    let mut mount_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for row in rows.iter().filter(|r| r.is_remote && r.alias.contains('/')) {
        let peer = row.alias.split('/').next().unwrap_or_default();
        if !peer.is_empty() {
            *mount_counts.entry(peer.to_string()).or_insert(0) += 1;
        }
    }
    let mut grouped: Vec<RepoRow> = Vec::with_capacity(rows.len() + 4);
    let mut headers_emitted: Vec<String> = Vec::new();
    for row in rows.drain(..) {
        if !row.is_remote {
            grouped.push(row);
            continue;
        }
        let peer = row.alias.split('/').next().unwrap_or_default().to_string();
        if !row.alias.contains('/')
            || peer.is_empty()
            || mount_counts.get(&peer).copied().unwrap_or(0) < 2
        {
            grouped.push(row);
            continue;
        }
        if !headers_emitted.contains(&peer) {
            headers_emitted.push(peer.clone());
            let host = hostname_of(&row.path, &peer);
            let mut header = row.clone();
            // The header names the federated peer AND its server: the remote
            // name is configuration, not a hostname, so show both —
            // `<remote-name>/<hostname>` (hostname-only fallback keeps one
            // name when the URL carries none).
            header.alias = if host == peer {
                host
            } else {
                format!("{peer}/{host}")
            };
            // Header rows are remote-shaped with EMPTY cells: the borrowed
            // stat columns of the first mount must not render as the
            // server's own — `stat_cell_texts` blanks them for
            // `group_header` rows (the status field itself is cleared here,
            // the status cell has no group_header gate).
            header.display_name = None;
            header.status = String::new();
            header.indent = false;
            header.group_header = true;
            grouped.push(header);
        }
        let mut child = row;
        child.indent = true;
        // The header already names the server; the mount shows the bare repo
        // name (display-only — `alias` remains `peer/repo` for actions/info).
        child.display_name = Some(
            child
                .alias
                .split_once('/')
                .map(|(_, rest)| rest.to_string())
                .unwrap_or_else(|| child.alias.clone()),
        );
        grouped.push(child);
    }
    *rows = grouped;
}

/// The collapse key of a row that ANCHORS a collapsible group: a local
/// worktree-family main (its alias — unindented, so a worktree child's own
/// `Some(main)` note never makes it an anchor of itself) or a peer-mount
/// header (the peer part of its alias). Child rows and ungrouped rows return
/// None.
fn group_anchor_key(row: &RepoRow) -> Option<String> {
    if row.is_remote {
        if row.group_header {
            Some(
                row.alias
                    .split('/')
                    .next()
                    .unwrap_or(&row.alias)
                    .to_string(),
            )
        } else {
            None
        }
    } else if !row.indent && row.family_note.is_some() {
        Some(row.alias.clone())
    } else {
        None
    }
}

/// Drop the child rows of collapsed groups. A group anchor (family main /
/// peer header) always stays visible so space can expand it again; an
/// indented child hides when its group scope — the nearest preceding anchor
/// — is collapsed; any unindented non-anchor row ends the current scope.
fn visible_rows(rows: &[RepoRow], collapsed: &std::collections::HashSet<String>) -> Vec<RepoRow> {
    let mut out = Vec::with_capacity(rows.len());
    let mut scope: Option<String> = None;
    for row in rows {
        if let Some(key) = group_anchor_key(row) {
            scope = Some(key);
        } else if !row.indent {
            scope = None;
        }
        let hidden = row.indent && scope.as_ref().is_some_and(|k| collapsed.contains(k));
        if !hidden {
            out.push(row.clone());
        }
    }
    out
}

/// The collapse key the row at `idx` toggles: its own anchor key, or — for an
/// indented child — the anchor key of the nearest preceding unindented row of
/// its own kind (local family main / remote peer header). None when the row
/// is ungrouped, so space is a no-op there.
fn toggle_group_key(rows: &[RepoRow], idx: usize) -> Option<String> {
    let row = rows.get(idx)?;
    if row.indent {
        rows[..idx]
            .iter()
            .rev()
            .find(|r| r.is_remote == row.is_remote && !r.indent)
            .and_then(group_anchor_key)
    } else {
        group_anchor_key(row)
    }
}

fn spawn_remote_discovery(
    state: Arc<ServeState>,
    tx: tokio::sync::mpsc::Sender<RemoteDiscoveryUpdate>,
    mut poke_rx: tokio::sync::mpsc::Receiver<String>,
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
        // Cadence of the CONFIG-ONLY row rebuild. This tick contacts no peer, so
        // it cannot wake a scale-to-zero replica and is safe to run often; it
        // exists purely so mount/unmount edits and `l` reloads surface promptly.
        let row_refresh = Duration::from_secs(crate::constants::REMOTE_ROW_REFRESH_SECS.max(1));

        // Cached per-(peer, remote_alias) status, retained across cycles so a
        // peer that blips this round keeps showing its last-known row.
        let mut status_lookup: std::collections::HashMap<
            (String, String),
            crate::federation::RemoteRepoStatus,
        > = std::collections::HashMap::new();
        // Per-peer wall-clock of the last successful `/status` refresh; shipped
        // with each snapshot so the render loop can mark stale activity.
        let mut refreshed_at: std::collections::HashMap<String, std::time::Instant> =
            std::collections::HashMap::new();

        loop {
            // ── Config-only snapshot. Rows come from the `remote_mounts`
            // allowlist and are merely *enriched* by whatever status is already
            // cached, so this issues no HTTP and cannot wake a sleeping peer. A
            // mount with no cached refresh (`refreshed_at` miss) renders its
            // activity as a stale `-`, which is the correct display for a peer
            // that is scaled to zero.
            //
            // Emitted UNCONDITIONALLY, including when no peers are configured:
            // the old code gated this on `!cfg.remotes.is_empty()`, so removing
            // the last peer from `repos.json` left the previously emitted rows
            // rendered forever (no snapshot was ever sent to clear them). With
            // no peers `build_remote_rows` yields an empty vec, which clears
            // them. Still zero HTTP, so this costs nothing.
            let cfg = state.config_snapshot();
            // Capacity-1 channel: replace the pending snapshot if the render
            // loop hasn't consumed it yet (try_send drops on Full — fine, the
            // next tick supersedes it anyway).
            let _ = tx.try_send(RemoteDiscoveryUpdate {
                rows: build_remote_rows(&status_lookup, &cfg),
                refreshed_at: refreshed_at.clone(),
            });

            // ── Wait: config-only tick OR an activity poke. ──
            // The tick merely loops back and re-emits rows. A poke means a real
            // federated tool call just landed on that peer, so it is provably
            // awake already — refresh that ONE peer; idle sibling peers are never
            // contacted. There is deliberately no timer branch that polls peers.
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(row_refresh) => {}
                peer = poke_rx.recv() => {
                    // poke_rx closes only when the render loop is shutting
                    // down (it owns poke_tx) → exit the discovery task.
                    let Some(first) = peer else { return; };
                    // Drain queued pokes; refresh each unique peer once.
                    let mut targets = std::collections::HashSet::new();
                    targets.insert(first);
                    while let Ok(more) = poke_rx.try_recv() {
                        targets.insert(more);
                    }
                    let cfg = state.config_snapshot();
                    for peer_name in targets {
                        let Some(peer) = cfg.remotes.get(&peer_name) else {
                            continue;
                        };
                        if let Some(repos) = poll_peer_status(&client, peer).await {
                            // Drop stale entries for this peer before inserting the
                            // fresh set (handles repos that vanished on the peer).
                            status_lookup.retain(|(p, _), _| p != &peer_name);
                            for r in repos {
                                status_lookup
                                    .insert((peer_name.clone(), r.alias.clone()), r);
                            }
                            refreshed_at.insert(peer_name, std::time::Instant::now());
                        }
                        // An unreachable peer keeps its cached row + aged refresh
                        // time (→ stale `-`), never vanishing from the table.
                    }
                    // Loop back: the snapshot at the top ships the refreshed rows.
                }
            }
        }
    });
}

/// Build the mounted remote-project rows from a cached `(peer, alias) → status`
/// map. Rows always come from the opt-in `remote_mounts` allowlist
/// (config-driven, so they always show); the status map only *enriches* them
/// with live per-repo state. A mount whose peer is unreachable simply falls back
/// to a "warm" default — discovery never defines which projects are mounted.
fn build_remote_rows(
    status_lookup: &std::collections::HashMap<
        (String, String),
        crate::federation::RemoteRepoStatus,
    >,
    cfg: &crate::db_discovery::repos::ReposConfig,
) -> Vec<RepoRow> {
    use crate::db_discovery::repos::Target;
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
                display_name: None,
                // Reuse the peer's own status vocabulary (open/warm/…); default
                // to "warm" for a cached-but-unreachable peer.
                status: st
                    .map(|s| s.status.clone())
                    .unwrap_or_else(|| "warm".to_string()),
                csharp_index: "none".to_string(),
                csharp_error: None,
                typescript_index: "none".to_string(),
                changes: st.map(|s| s.changes).unwrap_or(0),
                tool_call_count: st.and_then(|s| s.tool_call_count).unwrap_or(0),
                last_tool_call: st.and_then(|s| s.last_tool_call.clone()),
                lock_mode: st.map(|s| s.lock_mode.clone()).unwrap_or_default(),
                // Detail panel shows where the mount lives.
                path: peer.url.clone(),
                is_remote: true,
                // Computed per-tick by the render loop from the per-peer refresh
                // time; discovery itself leaves it fresh-neutral.
                activity_stale: false,
                // Mounted remote projects carry no local family identity.
                family_note: None,
                indent: false,
                group_header: false,
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

/// Humanize a symbol-index age (seconds) for the Info overlay; the
/// `u64::MAX` sentinel `index_age` returns for an unreadable index renders
/// as "unknown age", never as a number.
pub(crate) fn human_age_seconds(secs: u64) -> String {
    if secs == u64::MAX {
        return "unknown age".to_string();
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    if hours < 48 {
        return format!("{hours}h");
    }
    format!("{}d", hours / 24)
}

/// Build an `OverlayState::Info` by gathering live stats from SharedStores or metadata.
/// pub(crate) so the serve test module can pin the symbol-warnings wiring.
pub(crate) fn build_info_overlay(alias: &str, state: &Arc<ServeState>) -> Option<OverlayState> {
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

    // Durable symbol-index warnings, read fresh from the on-disk meta
    // (shared with the REST info surface so the two cannot drift).
    let symbol_warnings = state.collect_symbol_warnings(&db_path);
    // Per-language symbol-index status — the shared collector, so the TUI,
    // REST info and find_impact answers cannot disagree.
    let symbol_index = state.collect_symbol_index_status(&project_path, &db_path);

    Some(OverlayState::Info {
        alias: alias.to_string(),
        path: db_path.display().to_string(),
        chunks,
        files,
        max_chunk_id,
        db_size_human,
        model,
        dims,
        lock,
        index_age,
        symbol_warnings,
        symbol_index,
        copy_feedback: None,
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

/// Spawn a background task to build the local info overlay off the UI loop
/// and send it back via `tx` tagged with `gen`. An unresolvable alias sends
/// nothing: generations are monotonic, so the pending generation can never
/// match a later result and no stale overlay can apply.
fn spawn_info_overlay(
    alias: String,
    state: Arc<ServeState>,
    tx: tokio::sync::mpsc::Sender<(u64, OverlayState)>,
    gen: u64,
) {
    tokio::spawn(async move {
        if let Some(ov) = build_info_overlay(&alias, &state) {
            let _ = tx.send((gen, ov)).await;
        }
    });
}

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
                    path: info.path,
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
pub(crate) enum ReindexLaunch {
    /// Background reindex task spawned successfully.
    Started,
    /// A reindex was already running for this alias — request ignored.
    AlreadyRunning,
    /// Could not start (config error, unresolved alias, read-only, open error).
    Failed,
}

/// Spawn a background force reindex task for the given repo alias.
/// Follows the same flow as the HTTP `reindex_handler`.
pub(crate) fn spawn_force_reindex(alias: String, state: &Arc<ServeState>) -> ReindexLaunch {
    // Guard against concurrent reindex
    if !state.begin_indexing(&alias, IndexingOwner::Reindex) {
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
            state.end_indexing(&alias, IndexingOwner::Reindex);
            return ReindexLaunch::Failed;
        }
    };
    let project_path = match config.resolve(&alias) {
        Some(p) => p,
        None => {
            tracing::error!("Cannot resolve alias '{}' for force reindex", alias);
            state.end_indexing(&alias, IndexingOwner::Reindex);
            return ReindexLaunch::Failed;
        }
    };
    // Same guard as the HTTP reindex route: a force reindex opens the repo
    // write-mode and rebuilds its index, which for a read-only repo means both a
    // memory blow-up on a constrained replica and divergence from the index its
    // owning job publishes.
    if config.repo_read_only.get(&alias) == Some(&true) {
        tracing::warn!(
            "Refusing force reindex of '{}': marked read-only (repo_read_only) — its index is \
             owned by another writer",
            alias
        );
        state.end_indexing(&alias, IndexingOwner::Reindex);
        return ReindexLaunch::Failed;
    }
    drop(config); // release read lock

    let db_path = project_path.join(DB_DIR_NAME);

    // Stop FSW
    let stores = match state.stop_fsw(&alias) {
        Some(s) => s,
        None => {
            // Try to open stores (allow_create=true for recovery)
            let cancel = CancellationToken::new();
            match state.try_open_stores(&alias, &db_path, true, false, None) {
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
                    state.end_indexing(&alias, IndexingOwner::Reindex);
                    return ReindexLaunch::Failed;
                }
                Err(e) => {
                    tracing::error!("Cannot open stores for '{}': {}", alias, e);
                    state.end_indexing(&alias, IndexingOwner::Reindex);
                    return ReindexLaunch::Failed;
                }
            }
        }
    };

    let alias_bg = alias.clone();
    let state_bg = state.clone();
    let pool = state.embedding_pool();
    // Fresh cancellation token for this reindex task, registered in
    // `index_tasks` so `remove_repo` can cancel + await it (BUG1).
    let reindex_token = CancellationToken::new();
    let reindex_token_task = reindex_token.clone();
    // The task closure moves `db_path`; keep a copy for the tracked entry so a
    // removed repo's DB dir can be cleaned up after the task exits.
    let task_db_path = db_path.clone();
    let handle = tokio::spawn(async move {
        tracing::info!(
            "TUI: Force reindex for '{}': clearing stores and reindexing",
            alias_bg
        );

        match IndexManager::force_reindex_with_stores(
            &project_path,
            &db_path,
            &stores,
            None,
            &reindex_token_task,
            Some(&pool),
        )
        .await
        {
            Ok(()) => {
                tracing::info!("TUI: Force reindex complete for '{}'", alias_bg);
            }
            Err(e) => {
                if reindex_token_task.is_cancelled() {
                    // Cancellation (e.g. remove_repo ran mid-reindex): the repo
                    // is already being torn down by remove_repo — do NOT restart
                    // the FSW, which would resurrect the removed alias with a
                    // fresh, uncancellable task.
                    tracing::info!("TUI: Reindex cancelled for '{}': {}", alias_bg, e);
                    state_bg.end_indexing(&alias_bg, IndexingOwner::Reindex);
                    return;
                }
                tracing::error!("TUI: Force reindex failed for '{}': {}", alias_bg, e);
            }
        }

        // Guard: even if force_reindex returned Ok, the repo may have been
        // removed (or the task cancelled) during the embed pass. Do NOT restart
        // the FSW — that would resurrect the removed alias. restart_fsw's own
        // config check is insufficient here because remove_repo unregisters
        // config AFTER awaiting this task.
        if !state_bg.is_alias_live(&alias_bg, &reindex_token_task) {
            // Alias removed during force_reindex (whose final build_index is
            // uninterruptible). `remove_repo` gave up awaiting this task and
            // reported its own outcome; drop our stores handle (closes the
            // LMDB env) and self-clean the orphaned DB dir.
            tracing::info!(
                "TUI: Repo '{}' removed mid-reindex; dropping stores and self-cleaning DB dir",
                alias_bg
            );
            drop(stores);
            state_bg.self_clean_if_unregistered(&alias_bg, &db_path);
            state_bg.end_indexing(&alias_bg, IndexingOwner::Reindex);
            return;
        }

        // Shared tail: fresh FSW + FORCED symbol pass (the TUI never ran a
        // symbol pass at all before — `n` left find_impact on stale
        // references while the text index was brand new).
        super::finish_force_reindex(&state_bg, &alias_bg, &project_path, &db_path, stores).await;

        // Remove guard
        state_bg.end_indexing(&alias_bg, IndexingOwner::Reindex);
    });
    state.index_tasks.insert(
        alias,
        IndexTask {
            handle,
            token: reindex_token,
            db_path: task_db_path,
            started_at: Instant::now(),
        },
    );

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

#[cfg(test)]
mod family_note_tests {
    use super::*;

    /// map_repo_rows' family_note wiring: the main-tree row reads
    /// "+ N worktrees", the worktree row "worktree of <main>", an unrelated
    /// repo gets None (card #261 TODO 4; the note vocabulary itself is
    /// pinned by the family_note unit test in repos_tests.rs).
    #[test]
    fn map_repo_rows_annotates_family_members() {
        let (_tmp, main, wt, state) = crate::serve::tests::family_state();

        let info = crate::serve::RepoStatusInfo {
            status: crate::serve::RepoStateLabel::Warm,
            changes: 0,
            last_tool_call: None,
            tool_call_count: 0,
            csharp_index: crate::serve::CSharpIndexStatus::None,
            csharp_error: None,
            typescript_index: crate::serve::CSharpIndexStatus::None,
        };
        let repos = vec![
            (main.clone(), info),
            (
                wt.clone(),
                crate::serve::RepoStatusInfo {
                    status: crate::serve::RepoStateLabel::Warm,
                    changes: 0,
                    last_tool_call: None,
                    tool_call_count: 0,
                    csharp_index: crate::serve::CSharpIndexStatus::None,
                    csharp_error: None,
                    typescript_index: crate::serve::CSharpIndexStatus::None,
                },
            ),
            (
                "solo".to_string(),
                crate::serve::RepoStatusInfo {
                    status: crate::serve::RepoStateLabel::Warm,
                    changes: 0,
                    last_tool_call: None,
                    tool_call_count: 0,
                    csharp_index: crate::serve::CSharpIndexStatus::None,
                    csharp_error: None,
                    typescript_index: crate::serve::CSharpIndexStatus::None,
                },
            ),
        ];
        let rows = map_repo_rows(&repos, &state);
        // Tree grouping: the main's row is followed directly by its
        // worktree, indented; solo stays in place.
        assert_eq!(rows[0].alias, main, "main anchors the family group");
        assert_eq!(rows[1].alias, wt, "worktree renders under its main");
        assert_eq!(rows[2].alias, "solo");
        assert!(!rows[0].indent && !rows[2].indent);
        assert!(rows[1].indent, "worktree rows carry the | indent");
        let by_alias: std::collections::HashMap<&str, &RepoRow> =
            rows.iter().map(|r| (r.alias.as_str(), r)).collect();
        assert_eq!(
            by_alias[wt.as_str()].family_note,
            None,
            "the tree indent carries the relationship; note column stays count-only"
        );
        assert_eq!(by_alias[main.as_str()].family_note.as_deref(), Some("+ 1"));
        assert_eq!(by_alias["solo"].family_note, None);
    }

    #[test]
    fn map_repo_rows_reorders_worktrees_met_before_their_main() {
        let (_tmp, main, wt, state) = crate::serve::tests::family_state();

        let info = || crate::serve::RepoStatusInfo {
            status: crate::serve::RepoStateLabel::Warm,
            changes: 0,
            last_tool_call: None,
            tool_call_count: 0,
            csharp_index: crate::serve::CSharpIndexStatus::None,
            csharp_error: None,
            typescript_index: crate::serve::CSharpIndexStatus::None,
        };
        // Snapshot order puts the worktree BEFORE its main: the skip branch
        // must hold it back and re-emit it under the main.
        let repos = vec![
            (wt.clone(), info()),
            (main.clone(), info()),
            ("solo".to_string(), info()),
        ];
        let rows = map_repo_rows(&repos, &state);
        assert_eq!(rows[0].alias, main, "main anchors the family group");
        assert_eq!(rows[1].alias, wt, "worktree re-emitted under its main");
        assert_eq!(rows[2].alias, "solo", "non-family order preserved");
        assert!(rows[1].indent);
        assert!(!rows[0].indent && !rows[2].indent);
    }

    #[test]
    fn map_repo_rows_keeps_orphan_worktrees_indented_at_the_tail() {
        let (_tmp, _main, wt, state) = crate::serve::tests::family_state();

        let info = || crate::serve::RepoStatusInfo {
            status: crate::serve::RepoStateLabel::Warm,
            changes: 0,
            last_tool_call: None,
            tool_call_count: 0,
            csharp_index: crate::serve::CSharpIndexStatus::None,
            csharp_error: None,
            typescript_index: crate::serve::CSharpIndexStatus::None,
        };
        // The main is absent from the snapshot (e.g. evicted): the defensive
        // tail still renders the worktree, indented, never dropped.
        let repos = vec![("solo".to_string(), info()), (wt.clone(), info())];
        let rows = map_repo_rows(&repos, &state);
        assert_eq!(rows[0].alias, "solo");
        assert_eq!(rows[1].alias, wt, "orphan worktree survives at the tail");
        assert!(rows[1].indent, "orphan worktree stays indented");
    }

    #[test]
    fn group_peer_mounts_groups_multi_mount_peers_under_a_synthetic_header() {
        let row = |alias: &str, is_remote: bool, path: &str| RepoRow {
            alias: alias.to_string(),
            display_name: None,
            status: "warm".to_string(),
            csharp_index: "none".to_string(),
            csharp_error: None,
            typescript_index: "none".to_string(),
            changes: 0,
            tool_call_count: 0,
            last_tool_call: None,
            lock_mode: String::new(),
            path: path.to_string(),
            is_remote,
            activity_stale: false,
            family_note: None,
            indent: false,
            group_header: false,
        };
        let mut rows = vec![
            row("local-repo", false, ""),
            {
                // Live stat columns on the mount: the synthetic header must
                // clear every borrowed field, so the fixture can tell.
                let mut r = row("cloud/first", true, "https://codesearch.dlw.example:39725");
                r.changes = 5;
                r.tool_call_count = 7;
                r.last_tool_call = Some("14:32".to_string());
                r.lock_mode = "write".to_string();
                r
            },
            row("cloud/second", true, "https://codesearch.dlw.example:39725"),
            row("peer3/a", true, ""),
            row("peer3/b", true, ""),
            row("ipv6/x", true, "http://[::1]:39725"),
            row("ipv6/y", true, "http://[::1]:39725"),
            row("bare-name", true, ""),
            row("solo/only", true, "https://solo.example:39725"),
        ];
        group_peer_mounts(&mut rows);
        assert_eq!(
            rows.len(),
            12,
            "one synthetic header per multi-mount peer (cloud, peer3, ipv6); none for solo"
        );
        assert!(
            !rows[0].indent && rows[0].alias == "local-repo",
            "locals are never touched"
        );
        assert_eq!(
            rows[1].alias, "cloud/codesearch.dlw.example",
            "synthetic header names the federated peer AND its server hostname"
        );
        assert!(
            !rows[1].indent && rows[1].status.is_empty(),
            "header is an unindented blank row"
        );
        let [changes, calls, last, lock] = tui_common::stat_cell_texts(&rows[1]);
        assert!(
            changes.is_empty() && calls.is_empty() && last.is_empty() && lock.is_empty(),
            "header renders every borrowed stat cell EMPTY, got \
             [{changes}] [{calls}] [{last}] [{lock}]"
        );
        assert_eq!(rows[2].alias, "cloud/first");
        assert_eq!(
            rows[2].display_name.as_deref(),
            Some("first"),
            "mounts display the bare repo name; alias stays the query name"
        );
        assert!(
            rows[2].indent,
            "every mount of the peer indents under the header"
        );
        assert_eq!(rows[3].alias, "cloud/second");
        assert_eq!(rows[3].display_name.as_deref(), Some("second"));
        assert!(rows[3].indent, "no mount doubles as the anchor any more");
        assert_eq!(
            rows[4].alias, "peer3",
            "peer whose URL carries no host falls back to the remote name alone"
        );
        assert_eq!(
            rows[7].alias, "ipv6/[::1]",
            "IPv6-literal URLs parse to the bracketed address, not a truncated '['"
        );
        assert!(rows[5].indent && rows[6].indent, "peer3 mounts indent");
        assert_eq!(rows[5].display_name.as_deref(), Some("a"));
        assert!(rows[7].status.is_empty(), "ipv6 header is blank");
        assert!(rows[8].indent && rows[9].indent, "ipv6 mounts indent");
        assert_eq!(rows[8].display_name.as_deref(), Some("x"));
        assert_eq!(rows[10].alias, "bare-name");
        assert!(
            !rows[10].indent,
            "remotes without peer/alias shape stay put, after the groups"
        );
        assert_eq!(
            rows[11].alias, "solo/only",
            "single-mount peers keep the full alias"
        );
        assert!(
            !rows[11].indent && rows[11].display_name.is_none(),
            "single-mount peers get neither a header nor a display-name strip"
        );
    }
}

#[cfg(test)]
mod collapse_tests {
    use super::*;

    fn row(
        alias: &str,
        is_remote: bool,
        family_note: Option<&str>,
        indent: bool,
        group_header: bool,
    ) -> RepoRow {
        RepoRow {
            alias: alias.to_string(),
            display_name: None,
            status: "warm".to_string(),
            csharp_index: "none".to_string(),
            csharp_error: None,
            typescript_index: "none".to_string(),
            changes: 0,
            tool_call_count: 0,
            last_tool_call: None,
            lock_mode: String::new(),
            path: String::new(),
            is_remote,
            activity_stale: false,
            family_note: family_note.map(str::to_string),
            indent,
            group_header,
        }
    }

    fn collapsed_set(keys: &[&str]) -> std::collections::HashSet<String> {
        keys.iter().map(|k| k.to_string()).collect()
    }

    fn aliases(rows: &[RepoRow]) -> Vec<&str> {
        rows.iter().map(|r| r.alias.as_str()).collect()
    }

    #[test]
    fn visible_rows_hides_collapsed_family_worktrees_and_expands_again() {
        let rows = vec![
            row("main", false, Some("+ 2"), false, false),
            // Production shape: a worktree child carries `Some(<main>)` as its
            // own family_note (repos.rs family_note) — it must still hide
            // under its collapsed main instead of anchoring itself.
            row("wt-a", false, Some("main"), true, false),
            row("wt-b", false, Some("main"), true, false),
            row("solo", false, None, false, false),
        ];
        let got = visible_rows(&rows, &collapsed_set(&["main"]));
        assert_eq!(
            aliases(&got),
            vec!["main", "solo"],
            "collapsed family hides its worktrees; the main stays so space can expand it"
        );
        assert_eq!(
            aliases(&visible_rows(&rows, &collapsed_set(&[]))),
            vec!["main", "wt-a", "wt-b", "solo"],
            "expanding restores every row"
        );
    }

    #[test]
    fn visible_rows_hides_collapsed_peer_mounts_under_their_header() {
        let rows = vec![
            row("local", false, None, false, false),
            row("cloud/host.example", true, None, false, true),
            row("cloud/akeneo", true, None, true, false),
            row("cloud/erp", true, None, true, false),
            row("bare-name", true, None, false, false),
        ];
        let got = visible_rows(&rows, &collapsed_set(&["cloud"]));
        assert_eq!(
            aliases(&got),
            vec!["local", "cloud/host.example", "bare-name"],
            "collapsed peer hides its mounts; the header stays so space can expand it"
        );
    }

    #[test]
    fn toggle_group_key_resolves_anchors_children_and_ungrouped() {
        let rows = vec![
            row("main", false, Some("+ 1"), false, false),
            row("wt-a", false, Some("main"), true, false),
            row("solo", false, None, false, false),
            row("cloud/host", true, None, false, true),
            row("cloud/akeneo", true, None, true, false),
            row("bare-name", true, None, false, false),
        ];
        assert_eq!(
            toggle_group_key(&rows, 0).as_deref(),
            Some("main"),
            "a family main toggles its own group"
        );
        assert_eq!(
            toggle_group_key(&rows, 1).as_deref(),
            Some("main"),
            "a worktree walks back to its family main"
        );
        assert_eq!(toggle_group_key(&rows, 2), None, "a solo repo has no group");
        assert_eq!(
            toggle_group_key(&rows, 3).as_deref(),
            Some("cloud"),
            "a peer header toggles its peer group"
        );
        assert_eq!(
            toggle_group_key(&rows, 4).as_deref(),
            Some("cloud"),
            "a mount walks back to its peer header"
        );
        assert_eq!(
            toggle_group_key(&rows, 5),
            None,
            "an ungrouped remote has no group"
        );
    }
}
