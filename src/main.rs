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
mod mcp;
mod output;
mod rerank;
mod search;
mod server;
mod vectordb;
mod watch;

use anyhow::Result;
use std::fs::OpenOptions;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Wait for a CTRL-C / SIGINT signal (platform-specific).
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{self, SignalKind};
        let mut sig = unix::signal(SignalKind::interrupt()).unwrap();
        sig.recv().await;
    }
    #[cfg(windows)]
    {
        tokio::signal::ctrl_c().await.unwrap();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Check for quiet mode early (before tracing init)
    let args: Vec<String> = std::env::args().collect();
    let is_quiet = args.iter().any(|a| a == "-q" || a == "--quiet");
    let is_json = args.iter().any(|a| a == "--json");
    let is_verbose = args.iter().any(|a| a == "-v" || a == "--verbose");

    // Create cancellation token for graceful shutdown
    let cancel_token = CancellationToken::new();
    let cancel_clone = cancel_token.clone();

    // Spawn CTRL-C handler: first signal → graceful, second signal → force exit
    tokio::spawn(async move {
        // First CTRL-C: request graceful shutdown
        wait_for_signal().await;
        if !is_quiet && !is_json {
            eprintln!("\n🛑 Shutting down gracefully... (press Ctrl-C again to force)");
        }
        cancel_clone.cancel();

        // Second CTRL-C: force exit
        wait_for_signal().await;
        if !is_quiet && !is_json {
            eprintln!("\n⚠️  Force shutdown!");
        }
        std::process::exit(130);
    });

    // Skip tracing in quiet mode or JSON output
    if !is_quiet && !is_json {
        // Set up file logging for verbose mode
        if is_verbose {
            // Open log file in append mode
            let log_file = OpenOptions::new()
                .create(true)
                .append(true)
                .open("codesearch_debug.log")
                .expect("Failed to open codesearch_debug.log");

            // Initialize tracing with both console and file output
            tracing_subscriber::registry()
                .with(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "codesearch=debug".into()),
                )
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(std::io::stdout)
                        .with_ansi(true),
                )
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(log_file)
                        .with_ansi(false),
                )
                .init();

            info!(
                "Starting codesearch v{} (verbose mode - logging to codesearch_debug.log)",
                env!("CARGO_PKG_VERSION_FULL")
            );
        } else {
            // Normal tracing (console only)
            tracing_subscriber::registry()
                .with(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "codesearch=info".into()),
                )
                .with(tracing_subscriber::fmt::layer())
                .init();

            info!("Starting codesearch v{}", env!("CARGO_PKG_VERSION_FULL"));
        }
    }

    // Run CLI — for MCP/serve commands, cancel_token enables graceful shutdown.
    // For short-lived commands, the token is simply unused.
    cli::run(cancel_token).await
}
