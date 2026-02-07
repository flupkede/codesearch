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
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<()> {
    // Check for quiet mode early (before tracing init)
    let args: Vec<String> = std::env::args().collect();
    let is_quiet = args.iter().any(|a| a == "-q" || a == "--quiet");
    let is_json = args.iter().any(|a| a == "--json");
    let is_verbose = args.iter().any(|a| a == "-v" || a == "--verbose");
    
    // Set up CTRL-C handler (platform-specific)
    let ctrl_c = async {
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
    };
    
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
    
    // Handle CTRL-C gracefully with tokio::select!
    tokio::select! {
        _ = ctrl_c => {
            if !is_quiet && !is_json {
                println!("\n🛑 Interrupted by user");
            }
            std::process::exit(130); // Standard exit code for SIGINT
        }
        result = cli::run() => {
            result
        }
    }
}
