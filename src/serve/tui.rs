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

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};

use tokio_util::sync::CancellationToken;

use super::ServeState;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the fullscreen TUI.  Spawns as a tokio task from `run_serve`.
///
/// Returns `Ok(())` when the user presses `q` / `Ctrl-C`, or when the
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

        terminal.draw(|f| {
            let size = f.area();
            let chunks = Layout::vertical([
                Constraint::Length(3), // header
                Constraint::Min(3),    // body (table)
                Constraint::Length(1), // footer
            ])
            .split(size);

            render_header(f, chunks[0], serve_url);
            render_table(f, chunks[1], &repos, &mut table_state);
            render_footer(f, chunks[2], &repos, &table_state);
        })?;

        // Poll for key events
        let mut should_quit = false;
        while event::poll(poll_timeout)? {
            if let Event::Key(key) = event::read()? {
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
    match key.code {
        KeyCode::Char('q') => true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => true,
        _ => false,
    }
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
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            serve_url.to_string(),
            Style::default().fg(Color::White),
        ),
        Span::styled(
            format!("  {} ", now),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Center the title line vertically (area is 3 rows, title is 1 row)
    let centered = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(inner);
    f.render_widget(
        ratatui::widgets::Paragraph::new(title_line),
        centered[0],
    );
}

fn render_table(
    f: &mut ratatui::Frame,
    area: Rect,
    repos: &[(String, super::RepoStatusInfo)],
    table_state: &mut TableState,
) {
    let header_cells = ["Alias", "Status", "Changes", "Last Tool Call", "Lock"];
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
            let status_cell = status_cell(info.status);
            let changes_cell = Cell::from(format!("{}", info.changes))
                .style(Style::default().fg(Color::White));
            let tool_cell = Cell::from(
                info.last_tool_call
                    .as_deref()
                    .unwrap_or("—")
                    .to_string(),
            )
            .style(Style::default().fg(Color::DarkGray));
            // We don't have lock info in lightweight status, show status-derived value
            let lock_cell = lock_cell_from_status(info.status);

            Row::new(vec![
                Cell::from(alias.clone()).style(Style::default().fg(Color::White)),
                status_cell,
                changes_cell,
                tool_cell,
                lock_cell,
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Min(max_alias_w as u16 + 2),
            Constraint::Length(12),
            Constraint::Length(9),
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
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▶ ");

    f.render_stateful_widget(table, area, table_state);
}

fn render_footer(
    f: &mut ratatui::Frame,
    area: Rect,
    repos: &[(String, super::RepoStatusInfo)],
    table_state: &TableState,
) {
    let selected = table_state.selected().unwrap_or(0);
    let scroll_indicator = if repos.len() > 1 {
        format!("[{}/{}]", selected + 1, repos.len())
    } else {
        String::new()
    };

    let footer = Line::from(vec![
        Span::styled(" [q] quit  ", Style::default().fg(Color::DarkGray)),
        Span::styled("[↑↓] scroll  ", Style::default().fg(Color::DarkGray)),
        Span::styled(scroll_indicator, Style::default().fg(Color::Yellow)),
    ]);

    let footer_area = area.inner(Margin {
        vertical: 0,
        horizontal: 1,
    });
    f.render_widget(
        ratatui::widgets::Paragraph::new(footer),
        footer_area,
    );
}

// ---------------------------------------------------------------------------
// Cell styling helpers
// ---------------------------------------------------------------------------

fn status_cell(status: super::RepoStateLabel) -> Cell<'static> {
    use super::RepoStateLabel::*;
    match status {
        Open => Cell::from("✓ ready".to_string())
            .style(Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Warm => Cell::from("◐ warm".to_string())
            .style(Style::default().fg(Color::Yellow)),
        Readonly => Cell::from("◑ ro".to_string())
            .style(Style::default().fg(Color::Cyan)),
        Indexing => Cell::from("⟳ idx…".to_string())
            .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Closed => Cell::from("○ closed".to_string())
            .style(Style::default().fg(Color::DarkGray)),
        Error => Cell::from("✗ error".to_string())
            .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        NoIndex => Cell::from("— no idx".to_string())
            .style(Style::default().fg(Color::DarkGray)),
    }
}

/// Derive lock column from RepoStateLabel.
/// TODO: Once RepoStatusInfo gains a real `lock_mode` field, replace this heuristic.
fn lock_cell_from_status(status: super::RepoStateLabel) -> Cell<'static> {
    use super::RepoStateLabel::*;
    match status {
        Open => Cell::from("write".to_string()).style(Style::default().fg(Color::Cyan)),
        Warm => Cell::from("read".to_string()).style(Style::default().fg(Color::DarkGray)),
        Readonly => Cell::from("read".to_string()).style(Style::default().fg(Color::DarkGray)),
        Indexing => Cell::from("write".to_string()).style(Style::default().fg(Color::Cyan)),
        Closed => Cell::from("—".to_string()).style(Style::default().fg(Color::DarkGray)),
        Error => Cell::from("—".to_string()).style(Style::default().fg(Color::Red)),
        NoIndex => Cell::from("—".to_string()).style(Style::default().fg(Color::DarkGray)),
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
