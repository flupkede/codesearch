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
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Row, Table, TableState};

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
    /// Human-readable status: "open", "warm", "readonly", "closed", "indexing", "error", "no_index"
    pub status: String,
    /// C# index status: "none", "ready", "indexing", "error" (empty also treated as none)
    pub csharp_index: String,
    /// Optional C# error message
    pub csharp_error: Option<String>,
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
}

/// Modal overlay shown on top of the normal TUI content.
/// `Esc` dismisses it.
/// On-disk index stats for a mounted remote project, fetched on demand from the
/// peer's `GET /repos/{alias}/info` (the local instance has no local index for a
/// mount, so these live on the peer).
#[derive(Debug, Clone)]
pub struct RemoteIndexStats {
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
        chunks: usize,
        files: usize,
        max_chunk_id: u32,
        db_size_human: String,
        model: String,
        dims: usize,
        lock: String,
        index_age: String,
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

pub fn handle_key(key: KeyEvent, table_state: &mut TableState, row_count: usize) -> KeyAction {
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
        _ => KeyAction::None,
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
}

/// Handle key events when an overlay is active.
///
/// - `Esc` always dismisses.
/// - `Enter` or `'y'` confirms a `ConfirmRemove` overlay.
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
        _ => None,
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

pub fn render_table(
    f: &mut ratatui::Frame,
    area: Rect,
    repos: &[RepoRow],
    table_state: &mut TableState,
) {
    let header_cells = [
        "Alias",
        "Status",
        "Changes",
        "Calls",
        "Last Tool Call",
        "Lock",
    ];
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
            let extra = match r.csharp_index.as_str() {
                "ready" | "error" | "indexing" => 4,
                _ => 0,
            };
            r.alias.len() + extra
        })
        .max()
        .unwrap_or(10)
        .max(10);

    let rows: Vec<Row> = repos
        .iter()
        .map(|repo| {
            let status_cell = status_cell(&repo.status, &repo.csharp_index);
            let changes_str = if repo.changes > 99999 {
                " 99k+".to_string()
            } else {
                format!("{:>5}", repo.changes)
            };
            let changes_cell = Cell::from(changes_str).style(Style::default().fg(Color::White));
            let calls_cell = if repo.tool_call_count > 0 {
                Cell::from(format!("{:>5}", repo.tool_call_count))
                    .style(Style::default().fg(Color::Cyan))
            } else {
                Cell::from("    -".to_string()).style(Style::default().fg(Color::DarkGray))
            };
            let tool_cell = Cell::from(repo.last_tool_call.as_deref().unwrap_or("—").to_string())
                .style(Style::default().fg(Color::DarkGray));
            let lock_cell = lock_cell(&repo.lock_mode);

            // Alias text with optional C# indicator suffix, plus its base style.
            let (alias_text, mut alias_style) = match repo.csharp_index.as_str() {
                "ready" => (
                    format!("{} C#·", repo.alias),
                    Style::default().fg(Color::White),
                ),
                "error" => (
                    format!("{} C#!", repo.alias),
                    Style::default().fg(Color::Red),
                ),
                "indexing" => {
                    let s = if pulse_bright() {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    };
                    (format!("{} C#…", repo.alias), s)
                }
                _ => (repo.alias.clone(), Style::default().fg(Color::White)),
            };

            // Red bold alias if the repo is in an error state.
            if repo.status == "error" {
                alias_style = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
            }

            // Mounted remote projects render italic to signal they live on a
            // peer (cyan unless an error already claimed the color).
            if repo.is_remote {
                alias_style = alias_style.add_modifier(Modifier::ITALIC);
                if repo.status != "error" {
                    alias_style = alias_style.fg(Color::Cyan);
                }
            }

            let alias_cell = Cell::from(alias_text).style(alias_style);

            Row::new(vec![
                alias_cell,
                status_cell,
                changes_cell,
                calls_cell,
                tool_cell,
                lock_cell,
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(max_alias_w as u16 + 2),
            Constraint::Length(14),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Min(24),
            Constraint::Length(7),
        ],
    )
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

    // Mounted remote projects show italic here too (matches the table): cyan
    // normally, red when in error state so the color stays consistent with the
    // table's error highlight. Local rows are unchanged (white bold).
    let alias_style = if repo.is_remote {
        let color = if repo.status == "error" {
            Color::Red
        } else {
            Color::Cyan
        };
        Style::default()
            .fg(color)
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

    // Third item: last tool call
    if let Some(ref tool) = repo.last_tool_call {
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

    // Optional error line for C# errors
    let error_line = if repo.csharp_index == "error" {
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

    let right_len = cpu_str.len() + sessions_str.len() + 3 + "C# │ ".len();

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

    let right_line = Line::from(vec![
        csharp_indicator,
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
            chunks,
            files,
            max_chunk_id,
            db_size_human,
            model,
            dims,
            lock,
            index_age,
        } => {
            let title = format!(" {} — Index Info ", alias);
            let lines = vec![
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
                Line::from(""),
                Line::from(Span::styled(
                    "  [Esc] close",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            render_centered_modal(f, area, &title, lines);
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
/// [`render_centered_modal_with_border_color`].
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
    let content_height = lines.len() as u16 + 2; // +2 for border
    let max_line_w = lines.iter().map(|l| l.width() as u16).max().unwrap_or(20);
    let title_w = title.len() as u16;
    let content_width = (max_line_w + 4).max(title_w + 4).max(30);

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
    let inner = block.inner(modal_area);
    f.render_widget(block, modal_area);

    let content = ratatui::widgets::Paragraph::new(lines);
    f.render_widget(content, inner);
}

// ---------------------------------------------------------------------------
// Cell styling helpers
// ---------------------------------------------------------------------------

fn status_cell(status: &str, csharp: &str) -> Cell<'static> {
    let bright = pulse_bright();
    match status {
        "open" => match csharp {
            "ready" => Cell::from("✓ ready C#·  ".to_string()).style(
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            "indexing" => {
                if bright {
                    Cell::from("⟳ idx C#…    ".to_string()).style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    Cell::from("⟳ idx C#…    ".to_string())
                        .style(Style::default().fg(Color::DarkGray))
                }
            }
            "error" => Cell::from("✓ ready C#!  ".to_string())
                .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            _ => Cell::from("✓ ready      ".to_string()).style(
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        },
        "warm" => match csharp {
            "ready" => {
                Cell::from("◐ warm C#·   ".to_string()).style(Style::default().fg(Color::Yellow))
            }
            "indexing" => {
                if bright {
                    Cell::from("⟳ idx C#…    ".to_string()).style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    Cell::from("⟳ idx C#…    ".to_string())
                        .style(Style::default().fg(Color::DarkGray))
                }
            }
            "error" => {
                Cell::from("◐ warm C#!   ".to_string()).style(Style::default().fg(Color::Yellow))
            }
            _ => Cell::from("◐ warm       ".to_string()).style(Style::default().fg(Color::Yellow)),
        },
        "readonly" => {
            Cell::from("◑ ro         ".to_string()).style(Style::default().fg(Color::Cyan))
        }
        "indexing" => {
            if bright {
                match csharp {
                    "ready" => Cell::from("⟳ idx… C#·   ".to_string()).style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    "indexing" => Cell::from("⟳ idx… C#…   ".to_string()).style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    "error" => Cell::from("⟳ idx… C#!   ".to_string())
                        .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                    _ => Cell::from("⟳ idx…       ".to_string()).style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                }
            } else {
                match csharp {
                    "ready" => Cell::from("⟳ idx… C#·   ".to_string())
                        .style(Style::default().fg(Color::DarkGray)),
                    "indexing" => Cell::from("⟳ idx… C#…   ".to_string())
                        .style(Style::default().fg(Color::DarkGray)),
                    "error" => Cell::from("⟳ idx… C#!   ".to_string())
                        .style(Style::default().fg(Color::DarkGray)),
                    _ => Cell::from("⟳ idx…       ".to_string())
                        .style(Style::default().fg(Color::DarkGray)),
                }
            }
        }
        "closed" => Cell::from("○ closed     ".to_string()).style(Style::default().fg(Color::Gray)),
        "error" => Cell::from("✗ error      ".to_string())
            .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        "no_index" => {
            Cell::from("— no idx     ".to_string()).style(Style::default().fg(Color::Gray))
        }
        _ => Cell::from(format!("{:<14}", status)).style(Style::default().fg(Color::White)),
    }
}

fn lock_cell(lock_mode: &str) -> Cell<'static> {
    match lock_mode {
        "write" => Cell::from("write".to_string()).style(Style::default().fg(Color::Cyan)),
        "read" => Cell::from("read".to_string()).style(Style::default().fg(Color::White)),
        _ => Cell::from("—".to_string()).style(Style::default().fg(Color::Gray)),
    }
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
            _ => ("Warm".to_string(), Color::Yellow),
        },
        "readonly" => ("Readonly".to_string(), Color::Cyan),
        "closed" => ("Closed".to_string(), Color::Gray),
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
