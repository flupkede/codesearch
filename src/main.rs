mod bench;
mod cache;
mod chunker;
mod cli;
mod constants;
mod db_discovery;
mod embed;
mod file;
mod fts;
mod index;
mod logger;
mod mcp;
mod output;
mod rerank;
mod search;
mod server;
mod vectordb;
mod watch;

use anyhow::Result;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI to get loglevel (need this before tracing init)
    let args: Vec<String> = std::env::args().collect();
    let is_quiet = args.iter().any(|a| a == "-q" || a == "--quiet");
    let is_json = args.iter().any(|a| a == "--json");

    // Parse loglevel from args (default: info)
    let loglevel = args
        .iter()
        .position(|a| a == "-l" || a == "--loglevel")
        .and_then(|pos| args.get(pos + 1))
            .cloned()
        .unwrap_or_else(|| "info".to_string());

    // Validate loglevel
    let log_level = logger::LogLevel::parse(&loglevel).unwrap_or(logger::LogLevel::Info);
    let log_level_str = log_level.as_str();

    // Create cancellation token for async shutdown (MCP server, file watcher)
    let cancel_token = CancellationToken::new();
    let cancel_clone = cancel_token.clone();

    // CTRL-C handling via ctrlc crate (SetConsoleCtrlHandler on Windows, sigaction on Unix).
    // First press: graceful shutdown via CancellationToken. Second press: force exit.
    ctrlc::set_handler(move || {
        if constants::SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
            // Second CTRL-C: force exit
            eprintln!("\n⚠️  Force shutdown!");
            std::process::exit(130);
        }
        if !is_quiet && !is_json {
            eprintln!("\n🛑 Shutting down gracefully... (press Ctrl-C again to force)");
        }
        constants::SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
        cancel_clone.cancel();
    })
    .expect("Failed to set CTRL-C handler");

    // For MCP/serve commands: DON'T initialize tracing here.
    // init_logger() in cli/mod.rs will set up console+file logging as the FIRST
    // and ONLY global subscriber (you can only set it once per process).
    let is_mcp_or_serve = args.iter().any(|a| a == "mcp" || a == "serve");

    if !is_quiet && !is_json && !is_mcp_or_serve {
        // Console-only tracing for short-lived CLI commands (search, index, stats, etc.)
        // IMPORTANT: Use stderr — stdout is reserved for program output
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| format!("codesearch={}", log_level_str).into()),
            )
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .init();

        info!(
            "Starting codesearch v{} (loglevel: {})",
            env!("CARGO_PKG_VERSION_FULL"),
            log_level_str
        );
    }

    // Run CLI — for MCP/serve commands, cancel_token enables graceful shutdown.
    // For short-lived commands, the token is simply unused.
    cli::run(cancel_token).await
}
