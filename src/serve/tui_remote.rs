//! Standalone TUI that connects to a running `codesearch serve` via HTTP.
//!
//! Polls `GET /status` every second and renders the same ratatui layout
//! as the embedded TUI in `tui.rs`, using the shared `tui_common` framework.
//! This allows the TUI to be opened and closed independently of the server process.
//!
//! Actions (i=info, d=doctor, n=reindex, r=remove, l=reload) are routed to
//! HTTP endpoints on the serve.

use std::io;
use std::time::Duration;

use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::Terminal;

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::{self, EnterAlternateScreen};

use serde::Deserialize;

use super::tui_common::{self, KeyAction, OverlayKeyAction, OverlayState, RepoRow};

// ---------------------------------------------------------------------------
// Data model (HTTP JSON response)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct StatusResponse {
    version: String,
    repos: Vec<RepoInfo>,
    active_sessions: u64,
    cpu_percent: String,
    csharp_helper: bool,
    #[serde(default)]
    uptime_secs: u64,
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
    #[serde(default)]
    csharp_error: Option<String>,
    #[serde(default)]
    path: String,
}

impl RepoInfo {
    fn to_row(&self) -> RepoRow {
        RepoRow {
            alias: self.alias.clone(),
            status: self.status.clone(),
            csharp_index: self.csharp_index.clone(),
            csharp_error: self.csharp_error.clone(),
            changes: self.changes,
            tool_call_count: self.tool_call_count,
            last_tool_call: self.last_tool_call.clone(),
            lock_mode: self.lock_mode.clone(),
            path: self.path.clone(),
            // The standalone remote dashboard shows ONE peer's own repos (local
            // from that peer's view), not projects mounted into another instance.
            is_remote: false,
        }
    }
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
    tui_common::restore_terminal(&mut terminal)?;

    result
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

async fn run_remote_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    serve_url: &str,
) -> Result<()> {
    let mut table_state = ratatui::widgets::TableState::default();
    table_state.select(Some(0));

    let mut data: Option<StatusResponse> = None;
    let mut connection_errors: u32 = 0;

    let status_url = format!("{}/status", serve_url.trim_end_matches('/'));
    let poll_interval = Duration::from_secs(1);
    let key_poll_timeout = Duration::from_millis(100);

    // Optional overlay
    let mut overlay: Option<OverlayState> = None;

    // Channel for async doctor/info results, tagged with a request generation
    // so a late result from a dismissed or superseded request is discarded.
    let (doctor_tx, mut doctor_rx) = tokio::sync::mpsc::channel::<(u64, OverlayState)>(1);
    // Monotonic id of the most recent doctor/info request; bumped on every spawn.
    let mut doctor_gen: u64 = 0;

    // Single shared HTTP client reused for the status poll and all action
    // requests. reqwest::Client is cheap to clone and shares one connection
    // pool; per-request timeouts are still applied via `.timeout()`.
    let client = reqwest::Client::new();

    loop {
        // Fetch status from serve
        match client.get(&status_url).send().await {
            Ok(resp) if resp.status().is_success() => match resp.json::<StatusResponse>().await {
                Ok(d) => {
                    data = Some(d);
                    connection_errors = 0;
                }
                Err(_) => {
                    connection_errors += 1;
                }
            },
            _ => {
                connection_errors += 1;
            }
        }

        // Build RepoRows from HTTP data
        let rows: Vec<RepoRow> = data
            .as_ref()
            .map(|d| d.repos.iter().map(|r| r.to_row()).collect())
            .unwrap_or_default();

        // Clamp selection
        if !rows.is_empty() {
            let sel = table_state.selected().unwrap_or(0);
            if sel >= rows.len() {
                table_state.select(Some(rows.len() - 1));
            }
        }

        // Check if doctor/info result arrived. Only apply it if the user is
        // still waiting on the current request (spinner showing and generation
        // matches); otherwise it is stale — drain and drop it.
        if let Ok((gen, result)) = doctor_rx.try_recv() {
            if gen == doctor_gen && matches!(overlay, Some(OverlayState::DoctorRunning { .. })) {
                overlay = Some(result);
            }
        }

        // Render
        let version = data.as_ref().map(|d| d.version.as_str()).unwrap_or("?");
        let active = data.as_ref().map(|d| d.active_sessions).unwrap_or(0);
        let cpu = data.as_ref().map(|d| d.cpu_percent.as_str()).unwrap_or("—");
        let csharp_helper = data.as_ref().map(|d| d.csharp_helper).unwrap_or(false);
        let uptime_str = data
            .as_ref()
            .map(|d| tui_common::format_uptime_secs(d.uptime_secs))
            .unwrap_or_else(|| "Up  0s".to_string());

        terminal.draw(|f| {
            let size = f.area();
            let chunks = Layout::vertical([
                Constraint::Length(3), // header
                Constraint::Min(4),    // body (table)
                Constraint::Length(3), // detail panel
                Constraint::Length(1), // footer
            ])
            .split(size);

            if data.is_some() {
                tui_common::render_header(f, chunks[0], serve_url, version, true, &uptime_str);
                tui_common::render_table(f, chunks[1], &rows, &mut table_state);
                tui_common::render_detail(f, chunks[2], &rows, &table_state, 3);
                tui_common::render_footer(
                    f,
                    chunks[3],
                    &rows,
                    &table_state,
                    active,
                    cpu,
                    csharp_helper,
                    None,
                );
            } else {
                tui_common::render_header(f, chunks[0], serve_url, "?", true, &uptime_str);
                let msg = if connection_errors >= 3 {
                    "Connection lost — will retry..."
                } else {
                    "Connecting..."
                };
                let connecting = ratatui::widgets::Paragraph::new(ratatui::text::Line::from(vec![
                    ratatui::text::Span::styled(
                        format!(" {} ", msg),
                        ratatui::style::Style::default().fg(ratatui::style::Color::Yellow),
                    ),
                ]));
                f.render_widget(connecting, chunks[1]);
                tui_common::render_footer(f, chunks[3], &[], &table_state, 0, "—", false, None);
            }

            // Render overlay on top if active
            if let Some(ref ov) = overlay {
                tui_common::render_overlay(f, size, ov);
            }
        })?;

        // Poll for key events
        let mut should_quit = false;
        while event::poll(key_poll_timeout)? {
            if let Event::Key(key) = event::read()? {
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
                                    let serve = serve_url.to_string();
                                    let client = client.clone();
                                    tokio::spawn(async move {
                                        let remove_url = format!(
                                            "{}/repos/{}",
                                            serve.trim_end_matches('/'),
                                            alias
                                        );
                                        match client
                                            .delete(&remove_url)
                                            .timeout(Duration::from_secs(10))
                                            .send()
                                            .await
                                        {
                                            Ok(resp) if resp.status().is_success() => {
                                                tracing::info!(
                                                    "Remote TUI: Removed repo '{}'",
                                                    alias
                                                );
                                            }
                                            Ok(resp) => {
                                                tracing::warn!(
                                                    "Remote TUI: Remove '{}' returned HTTP {}",
                                                    alias,
                                                    resp.status()
                                                );
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    "Remote TUI: Remove '{}' failed: {}",
                                                    alias,
                                                    e
                                                );
                                            }
                                        }
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

                let action = tui_common::handle_key(key, &mut table_state, rows.len());
                match action {
                    KeyAction::Reload => {
                        let reload_url = format!("{}/reload", serve_url.trim_end_matches('/'));
                        let _ = client
                            .post(&reload_url)
                            .timeout(Duration::from_secs(3))
                            .send()
                            .await;
                    }
                    KeyAction::ShowInfo(idx) => {
                        if idx < rows.len() {
                            let alias = rows[idx].alias.clone();
                            // Fetch info via HTTP
                            let info_url =
                                format!("{}/repos/{}/info", serve_url.trim_end_matches('/'), alias);
                            overlay = Some(OverlayState::DoctorRunning {
                                alias: alias.clone(),
                            });
                            doctor_gen += 1;
                            let gen = doctor_gen;
                            let tx = doctor_tx.clone();
                            let client = client.clone();
                            tokio::spawn(async move {
                                let result = match client
                                    .get(&info_url)
                                    .timeout(Duration::from_secs(10))
                                    .send()
                                    .await
                                {
                                    Ok(resp) if resp.status().is_success() => {
                                        match resp.json::<InfoResponse>().await {
                                            Ok(info) => OverlayState::Info {
                                                alias,
                                                chunks: info.chunks,
                                                files: info.files,
                                                max_chunk_id: info.max_chunk_id,
                                                db_size_human: info.db_size_human,
                                                model: info.model,
                                                dims: info.dims,
                                                lock: info.lock,
                                                index_age: info.index_age,
                                            },
                                            Err(e) => OverlayState::Doctor {
                                                alias,
                                                results: vec![
                                                    format!("✗ Failed to parse info: {}", e),
                                                    String::new(),
                                                    "  [Esc] close".to_string(),
                                                ],
                                            },
                                        }
                                    }
                                    _ => {
                                        // Fallback: show basic info
                                        OverlayState::Doctor {
                                            alias,
                                            results: vec![
                                                "✗ Info endpoint not available".to_string(),
                                                "  (Upgrade serve to get /repos/{alias}/info)"
                                                    .to_string(),
                                                String::new(),
                                                "  [Esc] close".to_string(),
                                            ],
                                        }
                                    }
                                };
                                let _ = tx.send((gen, result)).await;
                            });
                        }
                    }
                    KeyAction::RunDoctor(idx) => {
                        if idx < rows.len() {
                            let alias = rows[idx].alias.clone();
                            overlay = Some(OverlayState::DoctorRunning {
                                alias: alias.clone(),
                            });
                            doctor_gen += 1;
                            let gen = doctor_gen;
                            let tx = doctor_tx.clone();
                            let serve = serve_url.to_string();
                            let client = client.clone();
                            tokio::spawn(async move {
                                let doctor_url = format!(
                                    "{}/repos/{}/doctor",
                                    serve.trim_end_matches('/'),
                                    alias
                                );
                                let result = match client
                                    .post(&doctor_url)
                                    .timeout(Duration::from_secs(60))
                                    .send()
                                    .await
                                {
                                    Ok(resp) if resp.status().is_success() => {
                                        match resp.json::<DoctorResponse>().await {
                                            Ok(d) => OverlayState::Doctor {
                                                alias,
                                                results: d.results,
                                            },
                                            Err(e) => OverlayState::Doctor {
                                                alias,
                                                results: vec![
                                                    format!("✗ Failed to parse doctor: {}", e),
                                                    String::new(),
                                                    "  [Esc] close".to_string(),
                                                ],
                                            },
                                        }
                                    }
                                    Ok(resp) => OverlayState::Doctor {
                                        alias,
                                        results: vec![
                                            format!("✗ Doctor failed: HTTP {}", resp.status()),
                                            String::new(),
                                            "  [Esc] close".to_string(),
                                        ],
                                    },
                                    Err(e) => OverlayState::Doctor {
                                        alias,
                                        results: vec![
                                            format!("✗ Doctor request failed: {}", e),
                                            String::new(),
                                            "  [Esc] close".to_string(),
                                        ],
                                    },
                                };
                                let _ = tx.send((gen, result)).await;
                            });
                        }
                    }
                    KeyAction::ForceReindex(idx) => {
                        if idx < rows.len() {
                            let alias = rows[idx].alias.clone();
                            let reindex_url = format!(
                                "{}/repos/{}/reindex?force=true",
                                serve_url.trim_end_matches('/'),
                                alias
                            );
                            // Fire-and-forget POST
                            let _ = client
                                .post(&reindex_url)
                                .timeout(Duration::from_secs(5))
                                .send()
                                .await;
                        }
                    }
                    KeyAction::RequestRemove(idx) => {
                        if idx < rows.len() {
                            let alias = rows[idx].alias.clone();
                            overlay = Some(OverlayState::ConfirmRemove { alias });
                        }
                    }
                    KeyAction::None => {}
                }
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
// HTTP response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct InfoResponse {
    chunks: usize,
    files: usize,
    max_chunk_id: u32,
    db_size_human: String,
    model: String,
    dims: usize,
    lock: String,
    index_age: String,
}

#[derive(Debug, Deserialize)]
struct DoctorResponse {
    results: Vec<String>,
}
