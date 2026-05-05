//! Standalone TUI that connects to a running `codesearch serve` via HTTP.
//!
//! Polls `GET /status` every second and renders the same ratatui layout
//! as the embedded TUI in `tui.rs`. This allows the TUI to be opened and
//! closed independently of the server process.

use std::io;
use std::time::Duration;

use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Row, Table, TableState};
use ratatui::Terminal;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};

use serde::Deserialize;

// ---------------------------------------------------------------------------
// Data model for the remote TUI
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct StatusResponse {
    version: String,
    repos: Vec<RepoInfo>,
    active_sessions: u64,
    cpu_percent: String,
}

#[derive(Debug, Default, Deserialize)]
struct RepoInfo {
    alias: String,
    status: String,
    lock_mode: String,
    changes: u64,
    last_tool_call: Option<String>,
    tool_call_count: u64,
    csharp_index: String,
}

#[derive(Debug, Default)]
struct RemoteTuiState {
    data: Option<StatusResponse>,
    connection_errors: u32,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the standalone remote TUI. Polls `GET {serve_url}/status` every second.
pub async fn run_remote_tui(serve_url: String) -> Result<()> {
    // Setup terminal
    crossterm::execute!(io::stdout(), EnterAlternateScreen)?;
    terminal::enable_raw_mode()?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let result = run_remote_tui_loop(&mut terminal, &serve_url).await;

    // Always restore terminal
    restore_terminal(&mut terminal)?;

    result
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

async fn run_remote_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    serve_url: &str,
) -> Result<()> {
    let mut table_state = TableState::default();
    table_state.select(Some(0));

    let mut remote_state = RemoteTuiState::default();

    let status_url = format!("{}/status", serve_url.trim_end_matches('/'));
    let poll_interval = Duration::from_secs(1);
    let key_poll_timeout = Duration::from_millis(100);

    loop {
        // Fetch status from serve
        match reqwest::get(&status_url).await {
            Ok(resp) if resp.status().is_success() => match resp.json::<StatusResponse>().await {
                Ok(data) => {
                    remote_state.data = Some(data);
                    remote_state.connection_errors = 0;
                }
                Err(_) => {
                    remote_state.connection_errors += 1;
                }
            },
            _ => {
                remote_state.connection_errors += 1;
            }
        }

        // Clamp selection
        if let Some(ref data) = remote_state.data {
            if !data.repos.is_empty() {
                let sel = table_state.selected().unwrap_or(0);
                if sel >= data.repos.len() {
                    table_state.select(Some(data.repos.len() - 1));
                }
            }
        }

        // Render
        terminal.draw(|f| {
            let size = f.area();
            let chunks = Layout::vertical([
                Constraint::Length(3), // header
                Constraint::Min(4),    // body (table)
                Constraint::Length(3), // detail panel
                Constraint::Length(1), // footer
            ])
            .split(size);

            match &remote_state.data {
                Some(data) => {
                    render_header(f, chunks[0], serve_url, &data.version);
                    render_table(f, chunks[1], &data.repos, &mut table_state);
                    render_detail(f, chunks[2], &data.repos, &table_state);
                    render_footer(
                        f,
                        chunks[3],
                        &data.repos,
                        &table_state,
                        data.active_sessions,
                        &data.cpu_percent,
                    );
                }
                None => {
                    render_header(f, chunks[0], serve_url, "?");
                    let msg = if remote_state.connection_errors >= 3 {
                        "Connection lost — will retry..."
                    } else {
                        "Connecting..."
                    };
                    let connecting =
                        ratatui::widgets::Paragraph::new(Line::from(vec![Span::styled(
                            format!(" {} ", msg),
                            Style::default().fg(Color::Yellow),
                        )]));
                    f.render_widget(connecting, chunks[1]);
                    render_footer(f, chunks[3], &[], &table_state, 0, "—");
                }
            }
        })?;

        // Poll for key events
        let mut should_quit = false;
        while event::poll(key_poll_timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if is_quit_key(key) {
                    should_quit = true;
                    break;
                }
                let repo_count = remote_state
                    .data
                    .as_ref()
                    .map(|d| d.repos.len())
                    .unwrap_or(0);
                handle_key(key, &mut table_state, repo_count);
            }
        }

        if should_quit {
            break;
        }

        tokio::time::sleep(poll_interval).await;
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
// Rendering — mirrors the embedded TUI in tui.rs
// ---------------------------------------------------------------------------

fn render_header(f: &mut ratatui::Frame, area: Rect, serve_url: &str, version: &str) {
    let now = chrono::Local::now().format("%H:%M:%S").to_string();

    let title_line = Line::from(vec![
        Span::styled(
            format!(" codesearch serve v{} · ", version),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(serve_url.to_string(), Style::default().fg(Color::White)),
        Span::styled(format!("  {} ", now), Style::default().fg(Color::DarkGray)),
        Span::styled("[remote]".to_string(), Style::default().fg(Color::Magenta)),
    ]);

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let centered = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);
    f.render_widget(ratatui::widgets::Paragraph::new(title_line), centered[0]);
}

fn render_table(
    f: &mut ratatui::Frame,
    area: Rect,
    repos: &[RepoInfo],
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
                "ready" => 4,    // " C#·"
                "error" => 4,    // " C#!"
                "indexing" => 5, // " C#⟳" or " C#·"
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
            let status_cell = status_cell(&repo.status);
            let changes_cell =
                Cell::from(format!("{}", repo.changes)).style(Style::default().fg(Color::White));
            let calls_cell = if repo.tool_call_count > 0 {
                Cell::from(format!("{}", repo.tool_call_count))
                    .style(Style::default().fg(Color::Cyan))
            } else {
                Cell::from("—".to_string()).style(Style::default().fg(Color::DarkGray))
            };
            let tool_cell = Cell::from(repo.last_tool_call.as_deref().unwrap_or("—").to_string())
                .style(Style::default().fg(Color::DarkGray));
            let lock_cell = lock_cell(&repo.lock_mode);

            // Alias cell with optional C# indicator
            let alias_cell = match repo.csharp_index.as_str() {
                "ready" => Cell::from(format!("{} C#·", repo.alias))
                    .style(Style::default().fg(Color::White)),
                "error" => {
                    Cell::from(format!("{} C#!", repo.alias)).style(Style::default().fg(Color::Red))
                }
                "indexing" => {
                    // Pulsing C# indicator during indexing
                    let bright = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis()
                        % 1000
                        < 500;
                    if bright {
                        Cell::from(format!("{} C#⟳", repo.alias)).style(
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        )
                    } else {
                        Cell::from(format!("{} C#·", repo.alias))
                            .style(Style::default().fg(Color::DarkGray))
                    }
                }
                _ => Cell::from(repo.alias.clone()).style(Style::default().fg(Color::White)),
            };

            // Red alias if the repo has errors
            let alias_cell = if repo.status == "error" {
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
            Constraint::Min(max_alias_w as u16 + 2),
            Constraint::Length(12),
            Constraint::Length(9),
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

fn render_detail(f: &mut ratatui::Frame, area: Rect, repos: &[RepoInfo], table_state: &TableState) {
    if repos.is_empty() {
        return;
    }

    let idx = table_state.selected().unwrap_or(0);
    if idx >= repos.len() {
        return;
    }

    let repo = &repos[idx];

    let status_label = match repo.status.as_str() {
        "open" => "Open",
        "warm" => "Warm (no FSW)",
        "readonly" => "Readonly",
        "closed" => "Closed",
        "indexing" => "Indexing…",
        "error" => "Error",
        "no_index" => "No Index",
        _ => &repo.status,
    };

    let detail_line = Line::from(vec![
        Span::styled(" ▶ ", Style::default().fg(Color::Yellow)),
        Span::styled(
            repo.alias.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::default()),
        Span::styled(status_label, Style::default().fg(Color::Cyan)),
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("lock: {}", repo.lock_mode),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let tool_str = repo.last_tool_call.as_deref().unwrap_or("—");
    let csharp_str = match repo.csharp_index.as_str() {
        "ready" => "  C#·",
        "error" => "  C#!",
        "indexing" => {
            let bright = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                % 1000
                < 500;
            if bright {
                "  C#⟳"
            } else {
                "  C#·"
            }
        }
        _ => "",
    };
    let csharp_color = match repo.csharp_index.as_str() {
        "ready" => Color::Green,
        "error" => Color::Red,
        "indexing" => {
            let bright = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                % 1000
                < 500;
            if bright {
                Color::Yellow
            } else {
                Color::DarkGray
            }
        }
        _ => Color::DarkGray,
    };
    let info_line = Line::from(vec![
        Span::styled("   changes:", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(" {}  ", repo.changes),
            Style::default().fg(Color::White),
        ),
        Span::styled("calls:", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(" {}  ", repo.tool_call_count),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled("last:", Style::default().fg(Color::DarkGray)),
        Span::styled(format!(" {}", tool_str), Style::default().fg(Color::White)),
        Span::styled(csharp_str, Style::default().fg(csharp_color)),
    ]);

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::Rgb(40, 40, 60)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let detail_chunks =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(inner);

    f.render_widget(
        ratatui::widgets::Paragraph::new(detail_line),
        detail_chunks[0],
    );
    f.render_widget(
        ratatui::widgets::Paragraph::new(info_line),
        detail_chunks[1],
    );
}

fn render_footer(
    f: &mut ratatui::Frame,
    area: Rect,
    repos: &[RepoInfo],
    table_state: &TableState,
    active: u64,
    cpu: &str,
) {
    let selected = table_state.selected().unwrap_or(0);
    let scroll_indicator = if repos.len() > 1 {
        format!("[{}/{}]", selected + 1, repos.len())
    } else {
        String::new()
    };

    let sessions_str = format!("Sessions: {}", active);
    let cpu_str = format!("CPU: {}", cpu);

    let right_len = cpu_str.len() + sessions_str.len() + 3;

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

    let right_line = Line::from(vec![
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

fn status_cell(status: &str) -> Cell<'static> {
    match status {
        "open" => Cell::from("✓ ready".to_string()).style(
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        "warm" => Cell::from("◐ warm".to_string()).style(Style::default().fg(Color::Yellow)),
        "readonly" => Cell::from("◑ ro".to_string()).style(Style::default().fg(Color::Cyan)),
        "indexing" => Cell::from("⟳ idx…".to_string()).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        "closed" => Cell::from("○ closed".to_string()).style(Style::default().fg(Color::Gray)),
        "error" => Cell::from("✗ error".to_string())
            .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        "no_index" => Cell::from("— no idx".to_string()).style(Style::default().fg(Color::Gray)),
        _ => Cell::from(status.to_string()).style(Style::default().fg(Color::White)),
    }
}

fn lock_cell(lock_mode: &str) -> Cell<'static> {
    match lock_mode {
        "write" => Cell::from("write".to_string()).style(Style::default().fg(Color::Cyan)),
        "read" => Cell::from("read".to_string()).style(Style::default().fg(Color::White)),
        _ => Cell::from("—".to_string()).style(Style::default().fg(Color::Gray)),
    }
}
