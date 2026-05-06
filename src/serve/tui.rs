//! ratatui-based TUI for `codesearch serve`.
//!
//! Replaces the old `print_dashboard()` eprintln approach with a fullscreen
//! alternate-screen TUI that renders a live status table without flickering.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Row, Table, TableState};
use ratatui::Terminal;

use crossterm::event::{self, Event, KeyCode, KeyEvent};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};

use tokio_util::sync::CancellationToken;

use super::ServeState;

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
    restore_terminal(&mut terminal)?;

    result
}

async fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: Arc<ServeState>,
    cancel_token: CancellationToken,
    serve_url: &str,
) -> io::Result<()> {
    // TUI-local state
    let mut table_state = TableState::default();
    table_state.select(Some(0));
    let tick_interval = Duration::from_millis(500);
    let poll_timeout = Duration::from_millis(100);

    // sysinfo System instance — must persist across frames so cpu_usage()
    // can compute a delta between refresh calls (first call always returns 0).
    let mut sys_system: Option<sysinfo::System> = None;

    // Main loop
    loop {
        // Draw the UI
        let repos = state.repo_statuses_lightweight();

        // Clamp selection
        if !repos.is_empty() {
            let sel = table_state.selected().unwrap_or(0);
            if sel >= repos.len() {
                table_state.select(Some(repos.len() - 1));
            }
        }

        // Load session count + CPU for footer
        let active = state.active_session_count();
        let cpu = cpu_usage_str(&mut sys_system);

        terminal.draw(|f| {
            let size = f.area();
            let chunks = Layout::vertical([
                Constraint::Length(3), // header
                Constraint::Min(4),    // body (table)
                Constraint::Length(4), // detail panel (selected repo info + optional error)
                Constraint::Length(1), // footer
            ])
            .split(size);

            render_header(f, chunks[0], serve_url);
            render_table(f, chunks[1], &repos, &mut table_state);
            render_detail(f, chunks[2], &repos, &table_state, &state);
            let csharp_helper = state
                .symbol_registry
                .get("csharp")
                .map(|i| i.is_available())
                .unwrap_or(false);
            render_footer(
                f,
                chunks[3],
                &repos,
                &table_state,
                active,
                &cpu,
                csharp_helper,
            );
        })?;

        // Poll for key events
        let mut should_quit = false;
        while event::poll(poll_timeout)? {
            if let Event::Key(key) = event::read()? {
                // On Windows, crossterm emits both Press and Release events.
                // Only act on Press to avoid double-stepping (scroll by 2).
                if key.kind != event::KeyEventKind::Press {
                    continue;
                }
                if is_quit_key(key) {
                    should_quit = true;
                    break;
                }
                handle_key(key, &mut table_state, repos.len());
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
// Restore helpers
// ---------------------------------------------------------------------------

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    terminal::disable_raw_mode()?;
    crossterm::execute!(io::stdout(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

fn is_quit_key(key: KeyEvent) -> bool {
    // Ctrl-C is intentionally NOT a quit key here. crossterm's raw mode delivers
    // it as a key event (ENABLE_PROCESSED_INPUT off on Windows / ISIG off on Unix),
    // so the OS-level ctrlc::set_handler in main.rs is bypassed while the TUI runs.
    // Treating Ctrl-C as quit was a foot-gun: a stray Ctrl-C in the wrong terminal
    // would tear down the whole serve process. Use `q` instead.
    matches!(key.code, KeyCode::Char('q'))
}

fn handle_key(key: KeyEvent, table_state: &mut TableState, row_count: usize) {
    if row_count == 0 {
        return;
    }
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            let i = table_state.selected().unwrap_or(0);
            if i > 0 {
                table_state.select(Some(i - 1));
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let i = table_state.selected().unwrap_or(0);
            if i < row_count - 1 {
                table_state.select(Some(i + 1));
            }
        }
        KeyCode::Home => {
            table_state.select(Some(0));
        }
        KeyCode::End => {
            table_state.select(Some(row_count - 1));
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render_header(f: &mut ratatui::Frame, area: Rect, serve_url: &str) {
    let now = chrono::Local::now().format("%H:%M:%S").to_string();
    let version = env!("CARGO_PKG_VERSION");

    let title_line = Line::from(vec![
        Span::styled(
            format!(" codesearch serve v{} · ", version),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(serve_url.to_string(), Style::default().fg(Color::White)),
        Span::styled(format!("  {} ", now), Style::default().fg(Color::DarkGray)),
    ]);

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Center the title line vertically (area is 3 rows, title is 1 row)
    let centered = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
    f.render_widget(ratatui::widgets::Paragraph::new(title_line), centered[0]);
}

fn render_table(
    f: &mut ratatui::Frame,
    area: Rect,
    repos: &[(String, super::RepoStatusInfo)],
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
        .map(|(a, _)| a.len())
        .max()
        .unwrap_or(10)
        .max(10);

    let rows: Vec<Row> = repos
        .iter()
        .map(|(alias, info)| {
            let status_cell = status_cell(info.status, info.csharp_index);
            // Keep left edge stable: fixed-width, left-aligned value.
            let changes_str = if info.changes > 99999 {
                " 99k+".to_string()
            } else {
                format!("{:>5}", info.changes)
            };
            let changes_cell = Cell::from(changes_str).style(Style::default().fg(Color::White));
            let calls_cell = if info.tool_call_count > 0 {
                Cell::from(format!("{:>5}", info.tool_call_count))
                    .style(Style::default().fg(Color::Cyan))
            } else {
                Cell::from("    -".to_string()).style(Style::default().fg(Color::DarkGray))
            };
            let tool_cell = Cell::from(info.last_tool_call.as_deref().unwrap_or("—").to_string())
                .style(Style::default().fg(Color::DarkGray));
            // We don't have lock info in lightweight status, show status-derived value
            let lock_cell = lock_cell_from_status(info.status);

            // Alias cell — plain name, no C# indicator
            let alias_cell = Cell::from(alias.clone()).style(Style::default().fg(Color::White));

            // Red alias if the repo has errors
            let alias_cell = if matches!(info.status, super::RepoStateLabel::Error) {
                alias_cell.style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
            } else {
                alias_cell
            };

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

fn render_detail(
    f: &mut ratatui::Frame,
    area: Rect,
    repos: &[(String, super::RepoStatusInfo)],
    table_state: &TableState,
    state: &Arc<ServeState>,
) {
    if repos.is_empty() {
        return;
    }

    let idx = table_state.selected().unwrap_or(0);
    if idx >= repos.len() {
        return;
    }

    let (alias, info) = &repos[idx];
    let config = state.config_snapshot();
    let path = config
        .resolve(alias)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "—".to_string());

    // Truncate path if too long for the area
    let max_path_len = (area.width as usize).saturating_sub(14);
    let display_path = if path.len() > max_path_len && max_path_len > 3 {
        format!("...{}", &path[path.len() - max_path_len + 3..])
    } else {
        path
    };

    let (status_label, status_color) = match info.status {
        super::RepoStateLabel::Open => match info.csharp_index {
            super::CSharpIndexStatus::Ready => ("Open C#·".to_string(), Color::Green),
            super::CSharpIndexStatus::Indexing => ("Index C#…".to_string(), Color::Yellow),
            super::CSharpIndexStatus::Error => ("Open C#!".to_string(), Color::Red),
            super::CSharpIndexStatus::None => ("Open".to_string(), Color::Green),
        },
        super::RepoStateLabel::Warm => match info.csharp_index {
            super::CSharpIndexStatus::Ready => ("Warm C#·".to_string(), Color::Yellow),
            super::CSharpIndexStatus::Indexing => ("Index C#…".to_string(), Color::Yellow),
            super::CSharpIndexStatus::Error => ("Warm C#!".to_string(), Color::Red),
            super::CSharpIndexStatus::None => ("Warm".to_string(), Color::Yellow),
        },
        super::RepoStateLabel::Readonly => ("Readonly".to_string(), Color::Cyan),
        super::RepoStateLabel::Closed => ("Closed".to_string(), Color::Gray),
        super::RepoStateLabel::Indexing => match info.csharp_index {
            super::CSharpIndexStatus::Indexing => ("Index C#…".to_string(), Color::Yellow),
            _ => ("Indexing…".to_string(), Color::Yellow),
        },
        super::RepoStateLabel::Error => ("Error".to_string(), Color::Red),
        super::RepoStateLabel::NoIndex => ("No Index".to_string(), Color::Gray),
    };

    let detail_line = Line::from(vec![
        Span::styled(" ▶ ", Style::default().fg(Color::Yellow)),
        Span::styled(
            alias.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::default()),
        Span::styled(status_label, Style::default().fg(status_color)),
        Span::styled("  ", Style::default()),
        Span::styled(display_path, Style::default().fg(Color::DarkGray)),
    ]);

    // Second line: changes + tool calls + last tool call
    let tool_str = info.last_tool_call.as_deref().unwrap_or("—");
    let info_line = Line::from(vec![
        Span::styled("   changes:", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(" {}  ", info.changes),
            Style::default().fg(Color::White),
        ),
        Span::styled("calls:", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(" {}  ", info.tool_call_count),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled("last:", Style::default().fg(Color::DarkGray)),
        Span::styled(format!(" {}", tool_str), Style::default().fg(Color::White)),
    ]);

    // Error line: shown only when C# index has an error
    let error_line = if matches!(info.csharp_index, super::CSharpIndexStatus::Error) {
        let err_msg = info
            .csharp_error
            .as_deref()
            .unwrap_or("Unknown error");
        // Truncate error message to fit the area
        let max_err_len = (area.width as usize).saturating_sub(12);
        let display_err = if err_msg.len() > max_err_len && max_err_len > 3 {
            format!("{}...", &err_msg[..max_err_len - 3])
        } else {
            err_msg.to_string()
        };
        Some(Line::from(vec![
            Span::styled("   ", Style::default()),
            Span::styled("⚠ ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
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
        f.render_widget(
            ratatui::widgets::Paragraph::new(err_line),
            detail_chunks[2],
        );
    }
}

fn render_footer(
    f: &mut ratatui::Frame,
    area: Rect,
    repos: &[(String, super::RepoStatusInfo)],
    table_state: &TableState,
    active: u64,
    cpu: &str,
    csharp_helper: bool,
) {
    let selected = table_state.selected().unwrap_or(0);
    let scroll_indicator = if repos.len() > 1 {
        format!("[{}/{}]", selected + 1, repos.len())
    } else {
        String::new()
    };

    let sessions_str = format!("Sessions: {}", active);
    let cpu_str = format!("CPU: {}", cpu);

    // Right side: CPU | Sessions
    let right_len = cpu_str.len() + sessions_str.len() + 3 + "C# │ ".len(); // C# label + " │ "

    let footer_inner = area.inner(Margin {
        vertical: 0,
        horizontal: 1,
    });
    let [left, right] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(right_len as u16 + 2)])
            .areas(footer_inner);

    let left_line = Line::from(vec![
        Span::styled("[q] quit  ", Style::default().fg(Color::DarkGray)),
        Span::styled("[↑↓] scroll  ", Style::default().fg(Color::DarkGray)),
        Span::styled(scroll_indicator, Style::default().fg(Color::Yellow)),
    ]);

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
// Cell styling helpers
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
    // Refresh CPUs once on creation so sys.cpus().len() is populated.
    let sys = sys_system.get_or_insert_with(|| {
        let mut s = System::new();
        s.refresh_cpu_list(sysinfo::CpuRefreshKind::nothing());
        s
    });

    // Refresh only our process (cpu)
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);

    match sys.process(pid) {
        Some(proc) => {
            // sysinfo reports cpu_usage() as total across all logical CPUs
            // (e.g. 474% on a 15-core machine). Divide by CPU count to get
            // a 0-100% value that makes sense to humans.
            let num_cpus = sys.cpus().len().max(1) as f32;
            let pct = proc.cpu_usage() / num_cpus;
            format!("{:.0}%", pct)
        }
        None => "—".into(),
    }
}

fn status_cell(status: super::RepoStateLabel, csharp: super::CSharpIndexStatus) -> Cell<'static> {
    use super::CSharpIndexStatus as CS;
    use super::RepoStateLabel::*;
    match status {
        Open => match csharp {
            CS::Ready => Cell::from("✓ ready C#·  ".to_string())
                .style(Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            CS::Indexing => Cell::from("⟳ idx C#…   ".to_string())
                .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            CS::Error => Cell::from("✓ ready C#!  ".to_string())
                .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            CS::None => Cell::from("✓ ready      ".to_string())
                .style(Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        },
        Warm => match csharp {
            CS::Ready => Cell::from("◐ warm C#·   ".to_string())
                .style(Style::default().fg(Color::Yellow)),
            CS::Indexing => Cell::from("⟳ idx C#…   ".to_string())
                .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            CS::Error => Cell::from("◐ warm C#!   ".to_string())
                .style(Style::default().fg(Color::Yellow)),
            CS::None => Cell::from("◐ warm       ".to_string())
                .style(Style::default().fg(Color::Yellow)),
        },
        Readonly => Cell::from("◑ ro         ".to_string()).style(Style::default().fg(Color::Cyan)),
        Indexing => match csharp {
            CS::Ready => Cell::from("⟳ idx… C#·  ".to_string())
                .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            CS::Indexing => Cell::from("⟳ idx C#…   ".to_string())
                .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            CS::Error => Cell::from("⟳ idx… C#!  ".to_string())
                .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            CS::None => Cell::from("⟳ indexing…  ".to_string()).style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        },
        Closed => Cell::from("○ closed     ".to_string()).style(Style::default().fg(Color::Gray)),
        Error => Cell::from("✗ error      ".to_string())
            .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        NoIndex => Cell::from("— no idx     ".to_string()).style(Style::default().fg(Color::Gray)),
    }
}

/// Derive lock column from RepoStateLabel.
/// TODO: Once RepoStatusInfo gains a real `lock_mode` field, replace this heuristic.
fn lock_cell_from_status(status: super::RepoStateLabel) -> Cell<'static> {
    use super::RepoStateLabel::*;
    match status {
        Open => Cell::from("write".to_string()).style(Style::default().fg(Color::Cyan)),
        Warm => Cell::from("read".to_string()).style(Style::default().fg(Color::White)),
        Readonly => Cell::from("read".to_string()).style(Style::default().fg(Color::White)),
        Indexing => Cell::from("write".to_string()).style(Style::default().fg(Color::Cyan)),
        Closed => Cell::from("—".to_string()).style(Style::default().fg(Color::Gray)),
        Error => Cell::from("—".to_string()).style(Style::default().fg(Color::Red)),
        NoIndex => Cell::from("—".to_string()).style(Style::default().fg(Color::Gray)),
    }
}

// ---------------------------------------------------------------------------
// TTY detection
// ---------------------------------------------------------------------------

/// Check if stdout is connected to a real terminal (TTY).
/// Returns `false` when piped, redirected, or running as a service.
pub fn is_tty() -> bool {
    // crossterm::terminal::size() returns Err when stdout is not a real terminal.
    // This covers piped, redirected, and service scenarios.
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
