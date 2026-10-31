//! Shared TUI framework used by both the embedded TUI (`tui.rs`) and the
//! remote TUI (`tui_remote.rs`).
//!
//! Contains:
//! - `RepoRow` — unified data shape for one repo row in the table
//! - `KeyAction` — key press actions
//! - `OverlayState` — modal overlays (info, doctor, running indicator)
//! - All rendering functions (header, table, detail, footer, overlays)
//! - Key handling logic

use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Row, Table, TableState, Wrap};

/// Format elapsed time since `started_at` as "Up xxd xxh xxm xxs".
/// Omits days/hours/minutes when zero.
pub fn format_uptime(started_at: std::time::Instant) -> String {
    format_uptime_secs(started_at.elapsed().as_secs())
}

/// Format a duration in seconds as "Up xxd xxh xxm xxs".
/// Omits days/hours/minutes when zero.
pub fn format_uptime_secs(total_secs: u64) -> String {
    let days = total_secs / 86400;
    let hours = (total_secs % 86400) / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{}d", days));
    }
    if hours > 0 || !parts.is_empty() {
        parts.push(format!("{:2}h", hours));
    }
    if mins > 0 || !parts.is_empty() {
        parts.push(format!("{:2}m", mins));
    }
    parts.push(format!("{:2}s", secs));

    format!("Up {}", parts.join(" "))
}

use crossterm::event::{KeyCode, KeyEvent};

// ---------------------------------------------------------------------------
// Unified data types
// ---------------------------------------------------------------------------

/// One row in the repo status table. Both TUI modes populate this.
#[derive(Debug, Clone)]
pub struct RepoRow {
    pub alias: String,
    /// Display-only alias override: when set, the table renders this instead
    /// of `alias` (peer mounts show the bare repo name under their hostname
    /// header, while `alias` stays the full `peer/repo` query name used by
    /// actions and the info lookup). `None` everywhere else.
    pub display_name: Option<String>,
    /// Human-readable status: "open", "warm", "readonly", "idle" (wire "idle"/"closed":
    /// index on disk, repo unloaded — resting, not indexing), "indexing", "error", "no_index"
    pub status: String,
    /// C# index status: "none", "ready", "indexing", "error" (empty also treated as none)
    pub csharp_index: String,
    /// Optional C# error message
    pub csharp_error: Option<String>,
    /// TypeScript index status: same vocabulary as `csharp_index`.
    pub typescript_index: String,
    /// Pending file changes detected by file watcher
    pub changes: u64,
    /// Total MCP tool calls since serve start
    pub tool_call_count: u64,
    /// Timestamp of last MCP tool call
    pub last_tool_call: Option<String>,
    /// Lock mode: "write", "read", or ""
    pub lock_mode: String,
    /// Resolved filesystem path (embedded TUI only, empty for remote)
    pub path: String,
    /// True when this row is a *mounted remote project* (lives on a federation
    /// peer, surfaced locally via `project=<peer>/<alias>`). Rendered italic to
    /// signal it is not a local index.
    pub is_remote: bool,
    /// True when this *remote* row's activity (`last_tool_call`) is considered
    /// stale by the embedded TUI — i.e. the peer's `/status` hasn't been
    /// refreshed within `REMOTE_ACTIVITY_FRESH_SECS`. There is no background
    /// poll that could refresh it: a federated peer is contacted only when a
    /// real tool call pokes an immediate refresh (or on the `i` keypress), so
    /// for an idle mount this is the *normal* steady state, not a fault. When
    /// stale, the activity column renders `-` instead of a possibly-hours-old
    /// "Xh ago". Always `false` for local repos (which carry live serve state)
    /// and for the standalone remote dashboard.
    pub activity_stale: bool,
    /// Worktree-family annotation rendered in the dim Worktree column:
    /// `"+ N worktrees"` on a family's main-tree row. Worktree rows carry
    /// `None` — their relationship renders as the `| `-indented tree shape
    /// under the main (see `indent`). Display-only — actions and
    /// identity use `alias`. `None` on remote rows and familyless repos.
    pub family_note: Option<String>,
    /// True when this row renders with the `| ` group indent: a worktree
    /// grouped under its family's main, or a peer mount grouped under its
    /// peer prefix (first mount of a peer is the unindented anchor row).
    /// Display-only.
    pub indent: bool,
    /// True when this row is a synthetic group header (a peer-mount header
    /// row): it names the group but owns none of the stat columns, so the
    /// table renders its changes/calls/last-call/lock cells EMPTY instead of
    /// the borrowed `-`/`0`/`—` placeholders.
    pub group_header: bool,
}

/// Actions returned by key handling.
#[derive(Debug)]
pub enum KeyAction {
    /// No action / key not recognized.
    None,
    /// User pressed `l` — reload repos config.
    Reload,
    /// User pressed `i` — show info overlay for repo at given index.
    ShowInfo(usize),
    /// User pressed `d` — run doctor for repo at given index.
    RunDoctor(usize),
    /// User pressed `n` — force reindex repo at given index.
    ForceReindex(usize),
    /// User pressed `r` — request removal of repo at given index (shows confirmation).
    RequestRemove(usize),
    /// User pressed space — toggle collapse/expand of the group (worktree
    /// family or peer-mount group) the row at given index belongs to.
    ToggleCollapse(usize),
}

/// Modal overlay shown on top of the normal TUI content.
/// `Esc` dismisses it.
/// On-disk index stats for a mounted remote project, fetched on demand from the
/// peer's `GET /repos/{alias}/info` (the local instance has no local index for a
/// mount, so these live on the peer).
#[derive(Debug, Clone)]
pub struct RemoteIndexStats {
    /// On-disk path of the index database on the PEER (not the local
    /// machine) — the peer's `.codesearch.db` directory for this repo.
    pub path: String,
    pub chunks: usize,
    pub files: usize,
    pub db_size_human: String,
    pub model: String,
}

/// Fetch state of a mounted remote project's index stats, so the info panel can
/// distinguish "still loading" from "peer unreachable / no info".
#[derive(Debug, Clone)]
pub enum RemoteStatsState {
    /// Fetch in flight — the panel shows a placeholder.
    Loading,
    /// Peer answered with stats.
    Ready(RemoteIndexStats),
    /// Peer unreachable or `/info` unavailable.
    Unavailable,
}

#[derive(Debug, Clone)]
pub enum OverlayState {
    /// Info modal: repo name, chunks, files, db size, model, dims, etc.
    Info {
        alias: String,
        /// On-disk path of this repo's index database (the `.codesearch.db`
        /// directory), so a user staring at the info panel can find it on
        /// disk without cross-referencing `repos.json`.
        path: String,
        chunks: usize,
        files: usize,
        max_chunk_id: u32,
        db_size_human: String,
        model: String,
        dims: usize,
        lock: String,
        index_age: String,
        /// Index-level symbol warnings read fresh from the on-disk meta
        /// (`scip_meta[index_warnings]`): build-environment failures the
        /// helper survived. Unlike the in-memory C# status these survive a
        /// serve restart — the durable record of a degraded index.
        symbol_warnings: Vec<String>,
        /// Per-language symbol-index status (ready/degraded/stale/
        /// restore-pending/absent/not-applicable) — the shared
        /// [`crate::symbols::symbol_index_status`] collector, so the TUI,
        /// REST info and find_impact answers cannot disagree.
        symbol_index: Vec<crate::symbols::SymbolIndexStatus>,
        /// Transient result of the `c` (copy warnings) key, shown as the
        /// overlay's last content line until it closes.
        copy_feedback: Option<String>,
    },
    /// Info modal for a *mounted remote project* (federation peer). Remote
    /// mounts have no local on-disk index, so the chunk/file/db-size/model stats
    /// are fetched on demand from the peer's `GET /repos/{alias}/info` and carried
    /// in `stats`. We also surface the federation coordinates (peer URL) plus the
    /// peer-reported live status carried on the `RepoRow`.
    RemoteInfo {
        alias: String,
        peer_url: String,
        status: String,
        lock: String,
        changes: u64,
        tool_call_count: u64,
        last_tool_call: Option<String>,
        /// On-disk index stats fetched from the peer (loading / ready / unavailable).
        stats: RemoteStatsState,
    },
    /// Doctor is running in background — show spinner.
    DoctorRunning { alias: String },
    /// Doctor results: per-check pass/warn/fail lines.
    Doctor { alias: String, results: Vec<String> },
    /// Confirmation dialog for removing a repo's index.
    ConfirmRemove { alias: String },
}

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

pub fn is_quit_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('q'))
}

pub fn handle_key(
    key: KeyEvent,
    table_state: &mut TableState,
    row_count: usize,
    page_size: usize,
) -> KeyAction {
    if row_count == 0 {
        return KeyAction::None;
    }
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            let i = table_state.selected().unwrap_or(0);
            if i > 0 {
                table_state.select(Some(i - 1));
            }
            KeyAction::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let i = table_state.selected().unwrap_or(0);
            if i < row_count - 1 {
                table_state.select(Some(i + 1));
            }
            KeyAction::None
        }
        KeyCode::PageUp => {
            let i = table_state.selected().unwrap_or(0);
            table_state.select(Some(page_target(i, page_size, row_count, false)));
            KeyAction::None
        }
        KeyCode::PageDown => {
            let i = table_state.selected().unwrap_or(0);
            table_state.select(Some(page_target(i, page_size, row_count, true)));
            KeyAction::None
        }
        KeyCode::Home => {
            table_state.select(Some(0));
            KeyAction::None
        }
        KeyCode::End => {
            table_state.select(Some(row_count - 1));
            KeyAction::None
        }
        KeyCode::Char('l') => KeyAction::Reload,
        KeyCode::Char('i') => {
            let idx = table_state.selected().unwrap_or(0);
            KeyAction::ShowInfo(idx)
        }
        KeyCode::Char('d') => {
            let idx = table_state.selected().unwrap_or(0);
            KeyAction::RunDoctor(idx)
        }
        KeyCode::Char('n') => {
            let idx = table_state.selected().unwrap_or(0);
            KeyAction::ForceReindex(idx)
        }
        KeyCode::Char('r') => {
            let idx = table_state.selected().unwrap_or(0);
            KeyAction::RequestRemove(idx)
        }
        KeyCode::Char(' ') => {
            let idx = table_state.selected().unwrap_or(0);
            KeyAction::ToggleCollapse(idx)
        }
        _ => KeyAction::None,
    }
}

/// Selection after a page move: `current` shifted by one page, clamped to
/// the row range. A page at or above the row count degenerates to Home/End.
fn page_target(current: usize, page_size: usize, row_count: usize, down: bool) -> usize {
    let step = page_size.max(1);
    if down {
        (current + step).min(row_count - 1)
    } else {
        current.saturating_sub(step)
    }
}

// ---------------------------------------------------------------------------
// Overlay key handling
// ---------------------------------------------------------------------------

/// Actions returned by overlay key handling.
#[derive(Debug)]
pub enum OverlayKeyAction {
    /// Dismiss the overlay (Esc pressed).
    Dismiss,
    /// Confirm the removal action (Enter or 'y' pressed on a ConfirmRemove overlay).
    ConfirmRemove,
    /// Copy the Info overlay's symbol warnings to the OS clipboard ('c').
    CopyWarnings,
}

/// Handle key events when an overlay is active.
///
/// - `Esc` always dismisses.
/// - `Enter` or `'y'` confirms a `ConfirmRemove` overlay.
/// - `'c'` copies the symbol warnings of an `Info` overlay.
/// - All other keys are ignored (returns `None`).
pub fn handle_overlay_key(key: KeyEvent, overlay: &OverlayState) -> Option<OverlayKeyAction> {
    match key.code {
        KeyCode::Esc => Some(OverlayKeyAction::Dismiss),
        KeyCode::Enter | KeyCode::Char('y') => {
            if matches!(overlay, OverlayState::ConfirmRemove { .. }) {
                Some(OverlayKeyAction::ConfirmRemove)
            } else {
                None
            }
        }
        KeyCode::Char('c') => {
            if matches!(overlay, OverlayState::Info { .. }) {
                Some(OverlayKeyAction::CopyWarnings)
            } else {
                None
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Copy-to-clipboard (Info overlay's `c` key)
// ---------------------------------------------------------------------------

/// The text the Info overlay's `c` key puts on the clipboard: a header
/// naming the repo, then every stored warning line verbatim — the copy is
/// the escape hatch for lines the terminal cannot fit, so it must carry
/// everything the index recorded.
pub fn symbol_warnings_copy_text(alias: &str, warnings: &[String]) -> String {
    let mut out = format!("{alias} — symbol index warnings ({}):", warnings.len());
    for warning in warnings {
        out.push('\n');
        out.push_str(warning);
    }
    out
}

/// Best-effort OS clipboard write with no new dependency: spawn the
/// platform helper with the text on stdin. `Err` carries the reason so the
/// overlay can show it — a silent no-op copy would read as success.
pub fn copy_text_to_clipboard(text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Child, Command, Stdio};

    fn spawn_helper(program: &str, args: &[&str]) -> Result<Child, String> {
        Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("{program}: {e}"))
    }

    fn spawn_clipboard_helper() -> Result<Child, String> {
        #[cfg(windows)]
        {
            // clip.exe decodes stdin with the OEM code page and would
            // mangle the em-dashes/ellipses the warning lines carry;
            // Set-Clipboard with an explicit UTF-8 stdin does not.
            spawn_helper(
                "powershell",
                &[
                    "-NoProfile",
                    "-Command",
                    "[Console]::InputEncoding = [System.Text.Encoding]::UTF8; $input | Set-Clipboard",
                ],
            )
        }
        #[cfg(target_os = "macos")]
        {
            spawn_helper("pbcopy", &[])
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            // No single standard clipboard tool on Linux: try the common
            // three in order and surface which (if any) is missing.
            let candidates: &[(&str, &[&str])] = &[
                ("wl-copy", &[]),
                ("xclip", &["-selection", "clipboard"]),
                ("xsel", &["--clipboard", "--input"]),
            ];
            let mut last_err = String::from("no clipboard tool found (tried wl-copy, xclip, xsel)");
            for (program, args) in candidates {
                match spawn_helper(program, args) {
                    Ok(child) => return Ok(child),
                    Err(e) => last_err = e,
                }
            }
            Err(last_err)
        }
    }

    let mut child = spawn_clipboard_helper()?;
    if let Some(mut stdin) = child.stdin.take() {
        // The write must not block the TUI's event loop: every helper
        // above exits right after stdin reaches EOF.
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| format!("write stdin: {e}"))?;
    }
    let status = child.wait().map_err(|e| format!("wait: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("clipboard helper exited with {status}"))
    }
}

/// Handle `OverlayKeyAction::CopyWarnings`: copy the Info overlay's symbol
/// warnings to the OS clipboard and record the outcome as the overlay's
/// feedback line. Shared by both TUI event loops.
pub fn apply_copy_warnings(overlay: &mut OverlayState) {
    apply_copy_warnings_with(overlay, copy_text_to_clipboard);
}

/// The [`apply_copy_warnings`] logic with the clipboard write injected, so
/// tests can pin both outcome feedback lines without touching the real OS
/// clipboard (which would be a machine side effect, not an assertion).
pub(crate) fn apply_copy_warnings_with(
    overlay: &mut OverlayState,
    clipboard: impl Fn(&str) -> Result<(), String>,
) {
    if let OverlayState::Info {
        alias,
        symbol_warnings,
        copy_feedback,
        ..
    } = overlay
    {
        *copy_feedback = if symbol_warnings.is_empty() {
            Some("no symbol warnings recorded".to_string())
        } else {
            let text = symbol_warnings_copy_text(alias, symbol_warnings);
            match clipboard(&text) {
                Ok(()) => Some("✓ symbol warnings copied to clipboard".to_string()),
                Err(e) => Some(format!("copy failed: {e}")),
            }
        };
    }
}

// ---------------------------------------------------------------------------
// Rendering — shared between embedded and remote TUI
// ---------------------------------------------------------------------------

/// Returns true during the "bright" phase of a ~1s pulse cycle (500ms bright, 500ms dim).
pub fn pulse_bright() -> bool {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        % 1000
        < 500
}

pub fn render_header(
    f: &mut ratatui::Frame,
    area: Rect,
    serve_url: &str,
    version: &str,
    is_remote: bool,
    uptime: &str,
) {
    let mut spans = vec![
        Span::styled(
            format!(" codesearch serve v{} · ", version),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(serve_url.to_string(), Style::default().fg(Color::White)),
        Span::styled(
            format!("  {} ", uptime),
            Style::default().fg(Color::DarkGray),
        ),
    ];

    if is_remote {
        spans.push(Span::styled(
            "[remote]".to_string(),
            Style::default().fg(Color::Magenta),
        ));
    }

    let title_line = Line::from(spans);

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let centered = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
    f.render_widget(ratatui::widgets::Paragraph::new(title_line), centered[0]);
}

/// The C# alias-column indicator: `(glyph, style)` when the status carries
/// one. Color belongs to the indicator only — degraded (warnings) reads as
/// yellow ⚠, a general error as fully red `!` — so a table full of
/// warnings-degraded repos does not read as half the fleet being down.
pub fn csharp_indicator(status: &str, pulse: bool) -> Option<(&'static str, Style)> {
    match status {
        "ready" => Some((" C#·", Style::default().fg(Color::White))),
        "partial" => Some((" C#⚠", Style::default().fg(Color::Yellow))),
        "error" => Some((
            " C#!",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        "indexing" => {
            let style = if pulse {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            Some((" C#…", style))
        }
        _ => None,
    }
}

/// The TypeScript alias-column indicator — same severity language as the C#
/// one (`ts_indicator("partial")` is yellow; TS has no live Partial source
/// yet, but the shared vocabulary must already agree).
pub fn ts_indicator(status: &str) -> Option<(&'static str, Style)> {
    match status {
        "ready" => Some((" TS·", Style::default().fg(Color::White))),
        "partial" => Some((" TS⚠", Style::default().fg(Color::Yellow))),
        "error" => Some((
            " TS!",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        "indexing" => Some((" TS…", Style::default().fg(Color::DarkGray))),
        _ => None,
    }
}

/// The Worktree column's text: the note as-is, ellipsis-truncated at `cap`
/// display columns, empty when the row has no note.
fn note_cell_text(note: Option<&str>, cap: usize) -> String {
    match note {
        Some(n) if n.len() > cap => {
            let cut: String = n.chars().take(cap - 1).collect();
            format!("{cut}…")
        }
        Some(n) => n.to_string(),
        None => String::new(),
    }
}

/// Middle-ellipsis: when `s` exceeds `cap` display columns, keep its head and
/// tail and put "..." in the middle — a clipped-from-the-right alias hides the
/// distinguishing suffix (deploy tags, worktree names), a middle cut keeps
/// both ends recognizable. Char-count based, same convention as the rest of
/// the table budgets.
fn middle_ellipsis_text(s: &str, cap: usize) -> String {
    let count = s.chars().count();
    if count <= cap {
        return s.to_string();
    }
    if cap < 5 {
        // Too narrow for a sensible "..." cut; hard-truncate.
        return s.chars().take(cap).collect();
    }
    let keep = cap - 3;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let head_text: String = s.chars().take(head).collect();
    let tail_text: String = s.chars().skip(count - tail).collect();
    format!("{head_text}...{tail_text}")
}

/// Table chrome above the data rows: the header row plus its
/// `bottom_margin(1)`. Page stepping subtracts it from the body height.
pub const TABLE_HEADER_ROWS: u16 = 2;

pub fn render_table(
    f: &mut ratatui::Frame,
    area: Rect,
    repos: &[RepoRow],
    table_state: &mut TableState,
) {
    // Family notes get their own dim column, capped so one very long main
    // alias cannot re-create the overflow; hidden entirely when no row has
    // a note (no worktrees registered).
    const NOTE_COL_CAP: usize = 30;
    let max_note_w = repos
        .iter()
        .filter_map(|r| r.family_note.as_deref().map(str::len))
        .max()
        .unwrap_or(0)
        .min(NOTE_COL_CAP);
    let show_notes = max_note_w > 0;

    let mut header_cells = vec!["Alias"];
    if show_notes {
        header_cells.push("Worktree");
    }
    header_cells.extend(["Status", "Changes", "Calls", "Last Tool Call", "Lock"]);
    let header = Row::new(
        header_cells
            .iter()
            .map(|h| Cell::from(*h).style(Style::default().add_modifier(Modifier::BOLD))),
    )
    .style(Style::default().fg(Color::White))
    .bottom_margin(1);

    let max_alias_w = repos
        .iter()
        .map(|r| {
            // Each indicator is a suffix like " C#·"; ⚠ (the partial glyph)
            // is double-width (unicode-width EAW=W) so its variant budgets
            // 6 display cols instead of 4. The family note renders in its
            // own column so a long "worktree of …" note can no longer blow
            // the alias budget past the terminal width (which made ratatui
            // clip every column from the left). Worktree rows budget the
            // `| ` tree indent too.
            let extra = indicator_cols(&r.csharp_index) + indicator_cols(&r.typescript_index);
            r.alias.len() + if r.indent { 2 } else { 0 } + extra
        })
        .max()
        .unwrap_or(10)
        .max(10);

    // On a narrow terminal the fixed columns after the alias can overflow the
    // width and ratatui hard-clips cell content from the right — the exact
    // decay the worktree-column fix chased. Cap the alias column to what the
    // terminal can afford (status keeps its Min(24) floor) and middle-ellipsis
    // the aliases that no longer fit, instead of letting ratatui clip them.
    // Ratatui inserts one space between columns (default column_spacing);
    // the alias cap must budget those gaps too, or the constraint total
    // exceeds the width and the solver squeezes the alias column anyway
    // (clipping the ellipsised tail from the right).
    let gaps_after_alias: usize = if show_notes { 6 } else { 5 };
    let fixed_after_alias: usize = if show_notes { max_note_w + 2 } else { 0 }
        + 14 // status
        + 7 // changes
        + 7 // calls
        + 7 // lock
        + gaps_after_alias;
    let alias_avail = (area.width as usize)
        .saturating_sub(fixed_after_alias)
        .saturating_sub(24) // the Min(24) tool column floor
        .max(14); // never squeeze the alias column below a usable floor
    let alias_content_cap = alias_avail.saturating_sub(2); // cell padding

    let note_cell = |note: Option<&str>| -> Cell<'static> {
        Cell::from(note_cell_text(note, NOTE_COL_CAP)).style(Style::default().fg(Color::DarkGray))
    };

    let rows: Vec<Row> = repos
        .iter()
        .map(|repo| {
            let status_cell = status_cell(&repo.status, &repo.csharp_index);
            let [changes_text, calls_text, last_text, lock_text] = stat_cell_texts(repo);
            let changes_cell = Cell::from(changes_text).style(Style::default().fg(Color::White));
            let calls_cell = Cell::from(calls_text).style(if repo.tool_call_count > 0 {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            });
            // Federated peers are polled on the slow idle-suspend cadence (no
            // longer every 30s) so they can scale to zero; between refreshes the
            // cached activity is stale and rendered as `-` rather than a
            // misleading "Xh ago". Local repos are always live
            // (`activity_stale == false`).
            let tool_cell = Cell::from(last_text).style(Style::default().fg(Color::DarkGray));
            let lock_cell = Cell::from(lock_text).style(match repo.lock_mode.as_str() {
                "write" => Style::default().fg(Color::Cyan),
                "read" => Style::default().fg(Color::White),
                _ => Style::default().fg(Color::Gray),
            });

            // Alias text stays NEUTRAL — color lives only on the language
            // indicators: yellow ⚠ for a degraded (warnings) index, fully red
            // ! for a general error. A table with red aliases reads as half
            // the fleet being down, which a warnings-degraded index is not.
            // Worktree rows indent as a tree under their family's main.
            let base_alias_style = if repo.is_remote {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::ITALIC)
            } else {
                Style::default().fg(Color::White)
            };
            let prefix = if repo.indent { "| " } else { "" };
            let extra = indicator_cols(&repo.csharp_index) + indicator_cols(&repo.typescript_index);
            let text_cap = alias_content_cap
                .saturating_sub(prefix.len() + extra)
                .max(4);
            let shown_alias = repo.display_name.as_deref().unwrap_or(&repo.alias);
            let alias_display = format!("{prefix}{}", middle_ellipsis_text(shown_alias, text_cap));
            let mut alias_spans: Vec<Span> = vec![Span::styled(alias_display, base_alias_style)];

            if let Some((glyph, style)) =
                csharp_indicator(repo.csharp_index.as_str(), pulse_bright())
            {
                alias_spans.push(Span::styled(glyph, style));
            }
            if let Some((glyph, style)) = ts_indicator(repo.typescript_index.as_str()) {
                alias_spans.push(Span::styled(glyph, style));
            }

            let alias_cell = Cell::from(Text::from(Line::from(alias_spans)));

            let mut cells = vec![alias_cell];
            if show_notes {
                cells.push(note_cell(repo.family_note.as_deref()));
            }
            cells.extend([status_cell, changes_cell, calls_cell, tool_cell, lock_cell]);

            Row::new(cells)
        })
        .collect();

    let mut constraints = vec![Constraint::Length(
        (max_alias_w as u16 + 2).min(alias_avail as u16),
    )];
    if show_notes {
        constraints.push(Constraint::Length(max_note_w as u16 + 2));
    }
    constraints.extend([
        Constraint::Length(14),
        Constraint::Length(7),
        Constraint::Length(7),
        Constraint::Min(24),
        Constraint::Length(7),
    ]);
    let table = Table::new(rows, constraints)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::NONE)
                .style(Style::default().bg(Color::Reset)),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::Rgb(30, 30, 50))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    f.render_stateful_widget(table, area, table_state);
}

/// Detail panel: 3-4 rows with alias, status, path, changes, tool calls, optional C# error.
/// `detail_height` is the allocated area height (3 for remote, 4 for embedded).
pub fn render_detail(
    f: &mut ratatui::Frame,
    area: Rect,
    repos: &[RepoRow],
    table_state: &TableState,
    detail_height: u16,
) {
    if repos.is_empty() {
        return;
    }

    let idx = table_state.selected().unwrap_or(0);
    if idx >= repos.len() {
        return;
    }

    let repo = &repos[idx];

    // Status label + color
    let (status_label, status_color) = detail_status_style(&repo.status, &repo.csharp_index);

    // Truncate path if too long for the area. Count by chars (not bytes) so a
    // multi-byte UTF-8 path (e.g. C:\Users\Müller\...) never panics on a slice
    // that lands inside a char boundary.
    let max_path_len = (area.width as usize).saturating_sub(20);
    let path_char_count = repo.path.chars().count();
    let display_path = if path_char_count > max_path_len && max_path_len > 3 {
        let tail: String = repo
            .path
            .chars()
            .skip(path_char_count - (max_path_len - 3))
            .collect();
        format!("...{tail}")
    } else if repo.path.is_empty() {
        String::new()
    } else {
        repo.path.clone()
    };

    // Mounted remote projects show italic here too (matches the table): cyan,
    // never red — alias text stays neutral on every surface; the status line
    // below carries the error severity. Local rows are unchanged (white bold).
    let alias_style = if repo.is_remote {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD | Modifier::ITALIC)
    } else {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    };
    let mut detail_spans = vec![
        Span::styled(" ▶ ", Style::default().fg(Color::Yellow)),
        Span::styled(repo.alias.clone(), alias_style),
        Span::styled("  ", Style::default()),
        Span::styled(status_label, Style::default().fg(status_color)),
    ];

    // Add path if present (embedded TUI)
    if !display_path.is_empty() {
        detail_spans.push(Span::styled("  ", Style::default()));
        detail_spans.push(Span::styled(
            display_path,
            Style::default().fg(Color::DarkGray),
        ));
    }

    let detail_line = Line::from(detail_spans);

    // Second line: lock + changes + tool calls
    let mut info_spans = vec![
        Span::styled("   ", Style::default()),
        Span::styled(
            format!("lock: {}  ", repo.lock_mode),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled("changes:", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(" {}  ", repo.changes),
            Style::default().fg(Color::White),
        ),
        Span::styled("calls:", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(" {}", repo.tool_call_count),
            Style::default().fg(Color::Cyan),
        ),
    ];

    // Third item: last tool call. For a stale federated row the cached value is
    // possibly hours old (the peer hasn't been polled since the slow baseline
    // cadence), so show `-` instead of a misleading age. Local repos are always
    // live (`activity_stale == false`).
    if repo.activity_stale {
        info_spans.push(Span::styled(
            "  last:",
            Style::default().fg(Color::DarkGray),
        ));
        info_spans.push(Span::styled(" -", Style::default().fg(Color::DarkGray)));
    } else if let Some(ref tool) = repo.last_tool_call {
        info_spans.push(Span::styled(
            "  last:",
            Style::default().fg(Color::DarkGray),
        ));
        info_spans.push(Span::styled(
            format!(" {}", tool),
            Style::default().fg(Color::White),
        ));
    }

    let info_line = Line::from(info_spans);

    // Optional error line for C# errors and degraded (partial) indexes
    let error_line = if matches!(repo.csharp_index.as_str(), "error" | "partial") {
        let err_msg = repo.csharp_error.as_deref().unwrap_or("Unknown error");
        const ERR_PREFIX_COLS: usize = 7;
        let max_err_chars = (area.width as usize).saturating_sub(ERR_PREFIX_COLS);
        let err_chars: Vec<char> = err_msg.chars().collect();
        let display_err = if err_chars.len() > max_err_chars && max_err_chars > 3 {
            let truncated: String = err_chars[..max_err_chars - 3].iter().collect();
            format!("{}...", truncated)
        } else {
            err_msg.to_string()
        };
        Some(Line::from(vec![
            Span::styled("   ", Style::default()),
            Span::styled(
                "⚠ ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(display_err, Style::default().fg(Color::Red)),
        ]))
    } else {
        None
    };

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::Rgb(40, 40, 60)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let constraints = if error_line.is_some() {
        vec![
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ]
    } else {
        vec![Constraint::Length(1), Constraint::Length(1)]
    };
    let detail_chunks = Layout::vertical(constraints).split(inner);

    f.render_widget(
        ratatui::widgets::Paragraph::new(detail_line),
        detail_chunks[0],
    );
    f.render_widget(
        ratatui::widgets::Paragraph::new(info_line),
        detail_chunks[1],
    );
    if let Some(err_line) = error_line {
        if detail_height > 3 {
            f.render_widget(ratatui::widgets::Paragraph::new(err_line), detail_chunks[2]);
        }
    }
}

/// Build the mnemonic hint spans for one footer action (e.g. `before="rei"`,
/// `key="n"`, `after="dex  "` → "rei**n**dex").
///
/// When `enabled` is false the whole hint is dimmed and struck through
/// (`CROSSED_OUT`) to signal the action does not apply to the current selection
/// — e.g. `doctor`/`reindex`/`remove` on a mounted remote project, which lives
/// on a federation peer and has no local index to act on.
fn hint(before: &str, key: &str, after: &str, enabled: bool) -> Vec<Span<'static>> {
    if enabled {
        let mut spans = Vec::new();
        if !before.is_empty() {
            spans.push(Span::styled(
                before.to_string(),
                Style::default().fg(Color::DarkGray),
            ));
        }
        spans.push(Span::styled(
            key.to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::UNDERLINED),
        ));
        spans.push(Span::styled(
            after.to_string(),
            Style::default().fg(Color::DarkGray),
        ));
        spans
    } else {
        // Disabled: no mnemonic underline, struck through so it reads as "not
        // available here" rather than "press this key".
        vec![Span::styled(
            format!("{before}{key}{after}"),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::CROSSED_OUT),
        )]
    }
}

// Footer renders many independent display fields (hints, scroll, sessions, CPU,
// C# indicator, transient flash); grouping them into a struct would add
// ceremony without improving clarity.
#[allow(clippy::too_many_arguments)]
pub fn render_footer(
    f: &mut ratatui::Frame,
    area: Rect,
    repos: &[RepoRow],
    table_state: &TableState,
    active: u64,
    cpu: &str,
    csharp_helper: bool,
    ts_helper: bool,
    flash: Option<&str>,
) {
    let selected = table_state.selected().unwrap_or(0);
    let scroll_indicator = if repos.len() > 1 {
        format!(" {}/{}", selected + 1, repos.len())
    } else {
        String::new()
    };

    let sessions_str = format!("Sessions: {}", active);
    let cpu_str = format!("CPU: {}", cpu);

    let right_len = cpu_str.len() + sessions_str.len() + 3 + "C# │ ".len() + "TS │ ".len();

    let footer_inner = area.inner(Margin {
        vertical: 0,
        horizontal: 1,
    });
    let [left, right] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(right_len as u16 + 2)])
            .areas(footer_inner);

    // A transient flash message (e.g. "reindex started") takes over the left
    // line while active, so the user gets immediate confirmation of an action
    // even before the status column updates on the next redraw.
    // Actions that operate on a *local* index (doctor/reindex/remove) don't
    // apply to a mounted remote project — those live on a federation peer. When
    // such a row is selected, render those hints disabled (struck through) so
    // it's clear which keys do anything on this selection.
    let is_remote_selected = repos.get(selected).map(|r| r.is_remote).unwrap_or(false);
    let local_only = !is_remote_selected;

    let left_line = if let Some(msg) = flash {
        Line::from(vec![Span::styled(
            msg.to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )])
    } else {
        let mut spans: Vec<Span> = Vec::new();
        spans.extend(hint("", "i", "nfo  ", true));
        spans.extend(hint("", "d", "octor  ", local_only));
        spans.extend(hint("rei", "n", "dex  ", local_only));
        spans.extend(hint("", "r", "emove  ", local_only));
        spans.extend(hint("re", "l", "oad  ", true));
        spans.extend(hint("", "␣", " collapse  ", true));
        spans.extend(hint("", "q", "uit  ", true));
        spans.push(Span::styled(
            "↑↓",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::UNDERLINED),
        ));
        spans.push(Span::styled(
            scroll_indicator,
            Style::default().fg(Color::Yellow),
        ));
        Line::from(spans)
    };

    let csharp_indicator = if csharp_helper {
        Span::styled("C# │ ", Style::default().fg(Color::Green))
    } else {
        Span::styled("C# │ ", Style::default().fg(Color::DarkGray))
    };

    let ts_indicator = if ts_helper {
        Span::styled("TS │ ", Style::default().fg(Color::Green))
    } else {
        Span::styled("TS │ ", Style::default().fg(Color::DarkGray))
    };

    let right_line = Line::from(vec![
        csharp_indicator,
        ts_indicator,
        Span::styled(cpu_str, Style::default().fg(Color::Green)),
        Span::styled(" │ ", Style::default().fg(Color::DarkGray)),
        Span::styled(sessions_str, Style::default().fg(Color::Cyan)),
    ]);

    f.render_widget(ratatui::widgets::Paragraph::new(left_line), left);
    f.render_widget(
        ratatui::widgets::Paragraph::new(right_line).right_aligned(),
        right,
    );
}

// ---------------------------------------------------------------------------
// Overlay rendering
// ---------------------------------------------------------------------------

/// Render the current overlay on top of the frame.
/// The table/footer remain visible outside the modal — only the modal area itself is cleared.
pub fn render_overlay(f: &mut ratatui::Frame, area: Rect, overlay: &OverlayState) {
    match overlay {
        OverlayState::Info {
            alias,
            path,
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
            copy_feedback,
        } => {
            let title = format!(" {} — Index Info ", alias);
            let lines = vec![
                Line::from(vec![
                    Span::styled("  Path:        ", Style::default().fg(Color::DarkGray)),
                    Span::styled(path.clone(), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  Chunks:      ", Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("{}", chunks), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  Files:       ", Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("{}", files), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  Max chunk ID:", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{}", max_chunk_id),
                        Style::default().fg(Color::White),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  DB size:     ", Style::default().fg(Color::DarkGray)),
                    Span::styled(db_size_human.clone(), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  Model:       ", Style::default().fg(Color::DarkGray)),
                    Span::styled(model.clone(), Style::default().fg(Color::Cyan)),
                ]),
                Line::from(vec![
                    Span::styled("  Dimensions:  ", Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("{}", dims), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  Lock:        ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        lock.clone(),
                        Style::default().fg(if lock == "write" {
                            Color::Cyan
                        } else {
                            Color::White
                        }),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  Index age:   ", Style::default().fg(Color::DarkGray)),
                    Span::styled(index_age.clone(), Style::default().fg(Color::White)),
                ]),
            ];
            let mut lines = lines;
            // Per-language symbol-index status — the same shared collector
            // find_impact answers carry. The chunk stats above say nothing
            // about the SCIP index; this line-up does.
            if !symbol_index.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "  Symbol index:",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )));
                for status in symbol_index {
                    let (color, note) = match status.state.as_str() {
                        "ready" => (Color::Green, ""),
                        "degraded" => (Color::Red, " (incomplete — see warnings)"),
                        "stale" => (
                            Color::Yellow,
                            " (another build produced it; rebuild queued)",
                        ),
                        "restore-pending" => (Color::Yellow, " (waiting out a NuGet restore)"),
                        _ => (Color::DarkGray, ""),
                    };
                    lines.push(Line::from(Span::styled(
                        format!(
                            "    {}: {} · {}{}",
                            status.language,
                            status.state,
                            super::tui::human_age_seconds(status.age_seconds),
                            note
                        ),
                        Style::default().fg(color),
                    )));
                }
            }
            // Durable record of a degraded symbol index: the build problems
            // the helper survived. Red — the table's `C#⚠` indicator is
            // yellow (degraded-index semantics stay the stronger red here).
            if !symbol_warnings.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "  ⚠ Symbol index warnings:",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )));
                for warning in symbol_warnings {
                    lines.push(Line::from(Span::styled(
                        format!("    {warning}"),
                        Style::default().fg(Color::Red),
                    )));
                }
            }
            if let Some(note) = copy_feedback {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!("  {note}"),
                    Style::default().fg(Color::Green),
                )));
            }
            // The footer rides the border's bottom title: wrapped warning
            // text can outgrow the terminal, and the key hints must stay
            // visible when the content above is clipped.
            render_centered_modal_with_footer(
                f,
                area,
                &title,
                Some(" [Esc] close · [c] copy warnings "),
                lines,
                Color::Cyan,
            );
        }
        OverlayState::RemoteInfo {
            alias,
            peer_url,
            status,
            lock,
            changes,
            tool_call_count,
            last_tool_call,
            stats,
        } => {
            let title = format!(" {} — Remote Mount ", alias);
            let last = last_tool_call.as_deref().unwrap_or("—");
            // Index stats live on the peer; render them (or a placeholder) right
            // after the status line so remote mounts get the same chunk/file/
            // db-size/model detail a local repo shows in its Info overlay.
            let stat_lines: Vec<Line> = match stats {
                RemoteStatsState::Ready(s) => vec![
                    Line::from(vec![
                        Span::styled("  Path (peer): ", Style::default().fg(Color::DarkGray)),
                        Span::styled(s.path.clone(), Style::default().fg(Color::White)),
                    ]),
                    Line::from(vec![
                        Span::styled("  Chunks:      ", Style::default().fg(Color::DarkGray)),
                        Span::styled(format!("{}", s.chunks), Style::default().fg(Color::White)),
                    ]),
                    Line::from(vec![
                        Span::styled("  Files:       ", Style::default().fg(Color::DarkGray)),
                        Span::styled(format!("{}", s.files), Style::default().fg(Color::White)),
                    ]),
                    Line::from(vec![
                        Span::styled("  DB size:     ", Style::default().fg(Color::DarkGray)),
                        Span::styled(s.db_size_human.clone(), Style::default().fg(Color::White)),
                    ]),
                    Line::from(vec![
                        Span::styled("  Model:       ", Style::default().fg(Color::DarkGray)),
                        Span::styled(s.model.clone(), Style::default().fg(Color::White)),
                    ]),
                ],
                RemoteStatsState::Loading => vec![Line::from(vec![
                    Span::styled("  Index:       ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        "fetching from peer…",
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    ),
                ])],
                RemoteStatsState::Unavailable => vec![Line::from(vec![
                    Span::styled("  Index:       ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        "stats unavailable from peer",
                        Style::default().fg(Color::Yellow),
                    ),
                ])],
            };
            let mut lines = vec![
                Line::from(vec![
                    Span::styled("  Peer URL:    ", Style::default().fg(Color::DarkGray)),
                    Span::styled(peer_url.clone(), Style::default().fg(Color::Cyan)),
                ]),
                Line::from(vec![
                    Span::styled("  Status:      ", Style::default().fg(Color::DarkGray)),
                    Span::styled(status.clone(), Style::default().fg(Color::White)),
                ]),
            ];
            lines.extend(stat_lines);
            lines.extend(vec![
                Line::from(vec![
                    Span::styled("  Lock:        ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        lock.clone(),
                        Style::default().fg(if lock == "write" {
                            Color::Cyan
                        } else {
                            Color::White
                        }),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  Changes:     ", Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("{}", changes), Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled("  Tool calls:  ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{}", tool_call_count),
                        Style::default().fg(Color::Cyan),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  Last call:   ", Style::default().fg(Color::DarkGray)),
                    Span::styled(last.to_string(), Style::default().fg(Color::White)),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "  Mounted from a federation peer (read-only view).",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                )),
                Line::from(Span::styled(
                    "  Doctor / reindex / remove don't apply to mounts.",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  [Esc] close",
                    Style::default().fg(Color::DarkGray),
                )),
            ]);
            render_centered_modal(f, area, &title, lines);
        }
        OverlayState::Doctor { alias, results } => {
            let title = format!(" {} — Doctor ", alias);
            let mut lines: Vec<Line> = results
                .iter()
                .map(|r| {
                    let color = if r.starts_with("✓") {
                        Color::Green
                    } else if r.starts_with("⚠") {
                        Color::Yellow
                    } else if r.starts_with("✗") {
                        Color::Red
                    } else {
                        Color::White
                    };
                    Line::from(Span::styled(format!("  {}", r), Style::default().fg(color)))
                })
                .collect();
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  [Esc] close",
                Style::default().fg(Color::DarkGray),
            )));
            render_centered_modal(f, area, &title, lines);
        }
        OverlayState::DoctorRunning { alias } => {
            let title = format!(" {} — Doctor ", alias);
            let lines = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "  ⏳ Running diagnostics...",
                    Style::default().fg(Color::Yellow),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "  [Esc] cancel",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            render_centered_modal(f, area, &title, lines);
        }
        OverlayState::ConfirmRemove { alias } => {
            let title = " ⚠ Delete Index ";
            let lines = vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("  Delete index for '{}'?", alias),
                    Style::default().fg(Color::White),
                )),
                Line::from(Span::styled(
                    "  This stops the watcher, removes the DB,",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    "  and unregisters the repo.",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  [Enter] ", Style::default().fg(Color::Red)),
                    Span::styled("confirm  ", Style::default().fg(Color::White)),
                    Span::styled("[Esc] ", Style::default().fg(Color::DarkGray)),
                    Span::styled("cancel", Style::default().fg(Color::White)),
                ]),
            ];
            render_centered_modal_with_border_color(f, area, title, lines, Color::Red);
        }
    }
}

/// Render a centered modal with a title and content lines.
/// Uses the default cyan border/title color; delegates to
/// [`render_centered_modal_with_footer`].
pub fn render_centered_modal(
    f: &mut ratatui::Frame,
    area: Rect,
    title: &str,
    lines: Vec<Line<'_>>,
) {
    render_centered_modal_with_border_color(f, area, title, lines, Color::Cyan);
}

/// Render a centered modal with a custom border color.
/// Same as `render_centered_modal` but allows overriding the border/title color.
pub fn render_centered_modal_with_border_color(
    f: &mut ratatui::Frame,
    area: Rect,
    title: &str,
    lines: Vec<Line<'_>>,
    border_color: Color,
) {
    render_centered_modal_with_footer(f, area, title, None, lines, border_color);
}

/// Wrapped-row count `content` needs at `inner_width`, mirroring ratatui's
/// greedy word wrapper (`Wrap { trim: false }`): the line's leading indent
/// and the separators between words occupy real columns on the current row,
/// words that do not fit start a new row, a word longer than the row
/// hard-splits, every line takes at least one row. A naive char ceiling
/// UNDERSHOOTS — a row that breaks early at a word boundary pushes its
/// remainder onto an extra row, and dropped indent columns do the same.
pub(crate) fn estimate_wrapped_height(content: &[Line<'_>], inner_width: u16) -> u16 {
    let w = inner_width.max(1) as usize;
    let mut total: u16 = 0;
    for line in content {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        let chars: Vec<char> = text.chars().collect();
        // trim: false preserves the leading indent on the first row.
        let mut i = chars.iter().take_while(|c| **c == ' ').count();
        let mut col = i;
        let mut rows: u16 = 1;
        while i < chars.len() {
            let mut sep = 0usize;
            while i < chars.len() && chars[i] == ' ' {
                sep += 1;
                i += 1;
            }
            if i >= chars.len() {
                break;
            }
            let start = i;
            while i < chars.len() && chars[i] != ' ' {
                i += 1;
            }
            let len = i - start;
            if col == 0 {
                // The word starts on a fresh row: only hard splits add rows.
                col = len;
            } else if col + sep + len <= w {
                col += sep + len;
            } else {
                rows += 1;
                col = len;
            }
            while col > w {
                rows += 1;
                col -= w;
            }
        }
        total += rows;
    }
    total
}

/// Render a centered modal with a title, content lines, an optional footer
/// (drawn as the block's bottom border title so it survives clipping) and a
/// custom border color. Content wraps instead of clipping at the modal
/// edge: warning lines routinely carry URLs wider than the terminal.
pub fn render_centered_modal_with_footer(
    f: &mut ratatui::Frame,
    area: Rect,
    title: &str,
    footer: Option<&str>,
    lines: Vec<Line<'_>>,
    border_color: Color,
) {
    let max_line_w = lines.iter().map(|l| l.width() as u16).max().unwrap_or(20);
    let title_w = title.len() as u16;
    let content_width = (max_line_w + 4).max(title_w + 4).max(30).min(area.width);

    let content_height = estimate_wrapped_height(&lines, content_width.saturating_sub(2)) + 2; // +2 for border

    // Center the modal
    let modal_area = Rect {
        x: area
            .width
            .saturating_sub(content_width)
            .saturating_div(2)
            .min(area.width.saturating_sub(content_width)),
        y: area
            .height
            .saturating_sub(content_height)
            .saturating_div(2)
            .min(area.height.saturating_sub(content_height)),
        width: content_width.min(area.width),
        height: content_height.min(area.height),
    };

    // Clear the modal area so no table text shows through the modal interior
    f.render_widget(ratatui::widgets::Clear, modal_area);

    let block = ratatui::widgets::Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            title,
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(Color::Rgb(20, 20, 35)));
    let block = match footer {
        Some(footer) => {
            block.title_bottom(Span::styled(footer, Style::default().fg(Color::DarkGray)))
        }
        None => block,
    };
    let inner = block.inner(modal_area);
    f.render_widget(block, modal_area);

    let content = ratatui::widgets::Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(content, inner);
}

// ---------------------------------------------------------------------------
// Cell styling helpers
// ---------------------------------------------------------------------------

fn status_cell(status: &str, csharp: &str) -> Cell<'static> {
    let (text, style) = status_cell_spec(status, csharp);
    Cell::from(text).style(style)
}

/// Pure (text, style) decision behind `status_cell`, kept separate so tests can
/// assert on it (0.30 `Cell::style` is a setter, not a getter).
fn status_cell_spec(status: &str, csharp: &str) -> (String, Style) {
    let bright = pulse_bright();
    match status {
        "open" => match csharp {
            "ready" => (
                "✓ ready C#·  ".to_string(),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            "indexing" => {
                if bright {
                    (
                        "⟳ idx C#…    ".to_string(),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    (
                        "⟳ idx C#…    ".to_string(),
                        Style::default().fg(Color::DarkGray),
                    )
                }
            }
            "error" => (
                "✓ ready C#!  ".to_string(),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            _ => (
                "✓ ready      ".to_string(),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        },
        "warm" => match csharp {
            "ready" => (
                "◐ warm C#·   ".to_string(),
                Style::default().fg(Color::Yellow),
            ),
            "indexing" => {
                if bright {
                    (
                        "⟳ idx C#…    ".to_string(),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    (
                        "⟳ idx C#…    ".to_string(),
                        Style::default().fg(Color::DarkGray),
                    )
                }
            }
            "error" => (
                "◐ warm C#!   ".to_string(),
                Style::default().fg(Color::Yellow),
            ),
            _ => (
                "◐ warm       ".to_string(),
                Style::default().fg(Color::Yellow),
            ),
        },
        "readonly" => (
            "◑ ro         ".to_string(),
            Style::default().fg(Color::Cyan),
        ),
        "indexing" => {
            if bright {
                match csharp {
                    "ready" => (
                        "⟳ idx… C#·   ".to_string(),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    "indexing" => (
                        "⟳ idx… C#…   ".to_string(),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    "error" => (
                        "⟳ idx… C#!   ".to_string(),
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ),
                    _ => (
                        "⟳ idx…       ".to_string(),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                }
            } else {
                match csharp {
                    "ready" => (
                        "⟳ idx… C#·   ".to_string(),
                        Style::default().fg(Color::DarkGray),
                    ),
                    "indexing" => (
                        "⟳ idx… C#…   ".to_string(),
                        Style::default().fg(Color::DarkGray),
                    ),
                    "error" => (
                        "⟳ idx… C#!   ".to_string(),
                        Style::default().fg(Color::DarkGray),
                    ),
                    _ => (
                        "⟳ idx…       ".to_string(),
                        Style::default().fg(Color::DarkGray),
                    ),
                }
            }
        }
        "idle" | "closed" => (
            "○ idle       ".to_string(),
            Style::default().fg(Color::Gray),
        ),
        "error" => (
            "✗ error      ".to_string(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        "no_index" => (
            "— no idx     ".to_string(),
            Style::default().fg(Color::Gray),
        ),
        _ => (format!("{:<14}", status), Style::default().fg(Color::White)),
    }
}

/// The four stat-column texts (changes, calls, last tool call, lock) for one
/// row, styled by the caller. A group-header row names the group but owns no
/// stats — every text comes back empty so the header renders blank cells
/// instead of the first mount's borrowed `0`/`-`/`—` placeholders.
pub(crate) fn stat_cell_texts(repo: &RepoRow) -> [String; 4] {
    if repo.group_header {
        return [const { String::new() }; 4];
    }
    [
        if repo.changes > 99999 {
            " 99k+".to_string()
        } else {
            format!("{:>5}", repo.changes)
        },
        if repo.tool_call_count > 0 {
            format!("{:>5}", repo.tool_call_count)
        } else {
            "    -".to_string()
        },
        if repo.activity_stale {
            "-".to_string()
        } else {
            repo.last_tool_call.as_deref().unwrap_or("—").to_string()
        },
        match repo.lock_mode.as_str() {
            "write" => "write".to_string(),
            "read" => "read".to_string(),
            _ => "—".to_string(),
        },
    ]
}

/// Derive status label and color for the detail panel.
fn detail_status_style(status: &str, csharp: &str) -> (String, Color) {
    let bright = pulse_bright();
    match status {
        "open" => match csharp {
            "ready" => ("Open C#·".to_string(), Color::Green),
            "indexing" => (
                "Index C#…".to_string(),
                if bright {
                    Color::Yellow
                } else {
                    Color::DarkGray
                },
            ),
            "error" => ("Open C#!".to_string(), Color::Red),
            "partial" => ("Open C#⚠".to_string(), Color::Yellow),
            _ => ("Open".to_string(), Color::Green),
        },
        "warm" => match csharp {
            "ready" => ("Warm C#·".to_string(), Color::Yellow),
            "indexing" => (
                "Index C#…".to_string(),
                if bright {
                    Color::Yellow
                } else {
                    Color::DarkGray
                },
            ),
            "error" => ("Warm C#!".to_string(), Color::Red),
            "partial" => ("Warm C#⚠".to_string(), Color::Yellow),
            _ => ("Warm".to_string(), Color::Yellow),
        },
        "readonly" => ("Readonly".to_string(), Color::Cyan),
        "idle" | "closed" => ("Idle".to_string(), Color::Gray),
        "indexing" => match csharp {
            "indexing" => (
                "Index C#…".to_string(),
                if bright {
                    Color::Yellow
                } else {
                    Color::DarkGray
                },
            ),
            _ => (
                "Indexing…".to_string(),
                if bright {
                    Color::Yellow
                } else {
                    Color::DarkGray
                },
            ),
        },
        "error" => ("Error".to_string(), Color::Red),
        "no_index" => ("No Index".to_string(), Color::Gray),
        _ => (status.to_string(), Color::White),
    }
}

/// Display width of one alias-column indicator suffix (`" C#·"` family).
/// The partial glyph ⚠ is double-width (unicode-width EAW=W), so that
/// variant budgets 6 display cols; the 1-col glyphs budget 4.
fn indicator_cols(status: &str) -> usize {
    match status {
        "ready" | "error" | "indexing" => 4,
        "partial" => 6,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Terminal helpers
// ---------------------------------------------------------------------------

/// Restore the terminal to its normal state: disable raw mode, leave the
/// alternate screen, and show the cursor. Shared by both the embedded and
/// remote TUI on exit.
pub fn restore_terminal(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
) -> std::io::Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_cell_text_truncates_at_cap_and_empties_without_note() {
        assert_eq!(note_cell_text(None, 30), "");
        assert_eq!(note_cell_text(Some("+ 2 worktrees"), 30), "+ 2 worktrees");
        let long = "worktree of wt-209169-deploy-20261023";
        let got = note_cell_text(Some(long), 30);
        assert_eq!(got.chars().count(), 30, "capped to the column width");
        assert!(got.ends_with('…'));
        assert!(long.starts_with(got.trim_end_matches('…')));
    }

    #[test]
    fn middle_ellipsis_keeps_head_and_tail() {
        assert_eq!(middle_ellipsis_text("short", 10), "short");
        let long = "storefront-209169-deploy-2";
        let got = middle_ellipsis_text(long, 15);
        assert_eq!(got.chars().count(), 15, "capped to the column width");
        assert!(got.contains("..."), "ellipsis sits in the middle");
        assert!(long.starts_with(got.split("...").next().unwrap()));
        assert!(long.ends_with(got.rsplit("...").next().unwrap()));
        // Too narrow for a sensible cut: hard-truncate.
        assert_eq!(middle_ellipsis_text("abcdef", 3), "abc");
    }

    #[test]
    fn status_cell_renders_idle_and_legacy_closed_as_idle() {
        let expected = (
            "○ idle       ".to_string(),
            Style::default().fg(Color::Gray),
        );
        assert_eq!(status_cell_spec("idle", "ready"), expected);
        assert_eq!(status_cell_spec("closed", ""), expected);
    }

    #[test]
    fn detail_status_style_maps_idle_and_legacy_closed_to_idle_label() {
        assert_eq!(
            detail_status_style("idle", ""),
            ("Idle".to_string(), Color::Gray)
        );
        assert_eq!(
            detail_status_style("closed", ""),
            ("Idle".to_string(), Color::Gray)
        );
    }

    /// A degraded (partial) C# index renders YELLOW with the ⚠ glyph in the
    /// detail panel — a warning, not an error; only a general error is red.
    #[test]
    fn detail_status_style_maps_partial_to_yellow_warning() {
        assert_eq!(
            detail_status_style("open", "partial"),
            ("Open C#⚠".to_string(), Color::Yellow)
        );
        assert_eq!(
            detail_status_style("warm", "partial"),
            ("Warm C#⚠".to_string(), Color::Yellow)
        );
        assert_eq!(
            detail_status_style("warm", "error").1,
            Color::Red,
            "a general error stays fully red"
        );
    }

    /// Alias-column indicators: the alias text is styled by the caller and
    /// stays neutral; degraded reads yellow, only a general error is red.
    #[test]
    fn alias_indicators_use_yellow_for_degraded_and_red_only_for_errors() {
        let (glyph, style) = csharp_indicator("partial", false).unwrap();
        assert_eq!(glyph, " C#⚠");
        assert_eq!(style, Style::default().fg(Color::Yellow));

        let (glyph, style) = csharp_indicator("error", false).unwrap();
        assert_eq!(glyph, " C#!");
        assert_eq!(
            style,
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        );

        let (glyph, style) = ts_indicator("partial").unwrap();
        assert_eq!(glyph, " TS⚠");
        assert_eq!(style, Style::default().fg(Color::Yellow));

        // The transient dim/pulse phase: indexing indicators must stay dim —
        // an inherited bright style here once turned the whole alias red.
        let (_, style) = csharp_indicator("indexing", false).unwrap();
        assert_eq!(style, Style::default().fg(Color::DarkGray));
        let (_, style) = ts_indicator("indexing").unwrap();
        assert_eq!(style, Style::default().fg(Color::DarkGray));
    }

    /// The alias-column width budget must cover the actually rendered
    /// suffixes: the partial glyph ⚠ is double-width (EAW=W), so " C#⚠" is
    /// 5 display cols — budgeting it at 4 (the 1-col glyph family) would
    /// clip the ⚠ exactly when a partial row defines the widest alias.
    #[test]
    fn indicator_cols_budgets_double_width_partial_glyph() {
        assert_eq!(indicator_cols("ready"), 4);
        assert_eq!(indicator_cols("error"), 4);
        assert_eq!(indicator_cols("indexing"), 4);
        assert_eq!(
            indicator_cols("partial"),
            6,
            "double-width glyph needs more than the 4-col family budget"
        );
        assert_eq!(indicator_cols("none"), 0);
    }

    #[test]
    fn unknown_status_falls_back_without_idle_treatment() {
        assert_ne!(
            status_cell_spec("future_state", "").1,
            Style::default().fg(Color::Gray)
        );
        assert_eq!(
            status_cell_spec("future_state", "").0,
            "future_state  ".to_string()
        );
        assert_eq!(
            detail_status_style("future_state", ""),
            ("future_state".to_string(), Color::White)
        );
    }

    #[test]
    fn handle_key_space_maps_to_toggle_collapse_of_the_selection() {
        use crossterm::event::KeyModifiers;
        let key = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty());
        let mut ts = TableState::default();
        ts.select(Some(3));
        assert!(matches!(
            handle_key(key, &mut ts, 10, 5),
            KeyAction::ToggleCollapse(3)
        ));
        // Empty table: no selection to toggle.
        let mut ts = TableState::default();
        assert!(matches!(handle_key(key, &mut ts, 0, 5), KeyAction::None));
    }

    #[test]
    fn page_target_moves_by_page_and_clamps_to_the_row_range() {
        assert_eq!(page_target(5, 3, 10, true), 8);
        assert_eq!(page_target(5, 3, 10, false), 2);
        // Clamp at both ends.
        assert_eq!(page_target(9, 3, 10, true), 9);
        assert_eq!(page_target(0, 3, 10, false), 0);
        // Page at or above the row count degenerates to Home/End.
        assert_eq!(page_target(5, 10, 10, true), 9);
        assert_eq!(page_target(5, 10, 10, false), 0);
        // Zero page size floors at one row, never stalls.
        assert_eq!(page_target(5, 0, 10, true), 6);
        assert_eq!(page_target(5, 0, 10, false), 4);
    }

    #[test]
    fn handle_key_page_up_down_move_the_selection_by_page() {
        use crossterm::event::KeyModifiers;
        let mut ts = TableState::default();
        ts.select(Some(7));
        handle_key(
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::empty()),
            &mut ts,
            20,
            5,
        );
        assert_eq!(ts.selected(), Some(2));
        handle_key(
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::empty()),
            &mut ts,
            20,
            5,
        );
        assert_eq!(ts.selected(), Some(7));
        // Oversized page behaves as Home/End.
        handle_key(
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::empty()),
            &mut ts,
            20,
            50,
        );
        assert_eq!(ts.selected(), Some(19));
        handle_key(
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::empty()),
            &mut ts,
            20,
            50,
        );
        assert_eq!(ts.selected(), Some(0));
    }
}
