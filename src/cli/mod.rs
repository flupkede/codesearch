use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

use crate::embed::ModelType;
use crate::search::SearchOptions;

/// Index subcommands
#[derive(Subcommand, Debug)]
pub enum IndexCommands {
    /// Add a repository to the index (creates local or global index)
    Add {
        /// Path to add (defaults to current directory)
        path: Option<PathBuf>,

        /// Create global index instead of local
        #[arg(short = 'g', long)]
        global: bool,
    },

    /// Remove the index (local or global, auto-detected)
    #[command(visible_alias = "rm")]
    Remove {
        /// Path to remove (defaults to current directory)
        path: Option<PathBuf>,
    },

    /// Show index status (local or global)
    List,
}

/// Fast, local semantic code search powered by Rust
#[derive(Parser, Debug)]
#[command(name = "codesearch")]
#[command(author, version = env!("CARGO_PKG_VERSION_FULL"), about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Set log level (error, warn, info, debug, trace)
    #[arg(short = 'l', long, global = true, default_value = "info")]
    pub loglevel: String,

    /// Suppress informational output (only show results/errors)
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Override default store name
    #[arg(long, global = true)]
    pub store: Option<String>,

    /// Embedding model to use (e.g., bge-small, minilm-l6-q, jina-code)
    /// Available: minilm-l6, minilm-l6-q, minilm-l12, minilm-l12-q, paraphrase-minilm,
    ///            bge-small, bge-small-q, bge-base, nomic-v1, nomic-v1.5, nomic-v1.5-q,
    ///            jina-code, e5-multilingual, mxbai-large, modernbert-large
    #[arg(long, global = true)]
    pub model: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Search the codebase using natural language
    Search {
        /// Search query (e.g., "where do we handle authentication?")
        query: String,

        /// Maximum total results to return
        #[arg(short = 'm', long, default_value = "25")]
        max_results: usize,

        /// Maximum matches to show per file
        #[arg(long, default_value = "1")]
        per_file: usize,

        /// Show full chunk content instead of snippets
        #[arg(short, long)]
        content: bool,

        /// Show relevance scores
        #[arg(long)]
        scores: bool,

        /// Show file paths only (like grep -l)
        #[arg(long)]
        compact: bool,

        /// Force re-index changed files before searching
        #[arg(short, long)]
        sync: bool,

        /// Output JSON for agents
        #[arg(long)]
        json: bool,

        /// Path to search in (defaults to current directory)
        #[arg(long)]
        path: Option<PathBuf>,

        /// Use vector-only search (disable hybrid FTS)
        #[arg(long)]
        vector_only: bool,

        /// RRF k parameter for score fusion (default 20)
        #[arg(long, default_value = "20")]
        rrf_k: f32,

        /// Enable neural reranking for better accuracy (uses Jina Reranker)
        #[arg(long)]
        rerank: bool,

        /// Number of top results to rerank (default 50)
        #[arg(long, default_value = "50")]
        rerank_top: usize,

        /// Filter results to files under this path (e.g., "src/")
        #[arg(long)]
        filter_path: Option<String>,
    },

    /// Index the repository or manage global index registry
    Index {
        /// Path to index (defaults to current directory), or use "list" to show status
        path: Option<PathBuf>,

        /// Show what would be indexed without actually indexing
        #[arg(long)]
        dry_run: bool,

        /// Force full re-index
        #[arg(short = 'f', long, alias = "full")]
        force: bool,

        /// Add a repository to the index (creates local or global index)
        #[arg(long)]
        add: bool,

        /// Create global index instead of local (only with --add)
        #[arg(short = 'g', long)]
        global: bool,

        /// Remove the index (local or global, auto-detected)
        #[arg(long, visible_alias = "rm")]
        remove: bool,

        /// Show index status (local or global)
        #[arg(long)]
        list: bool,
    },

    /// Run a background server with live file watching
    Serve {
        /// Port to listen on
        #[arg(short, long, default_value = "4444")]
        port: u16,

        /// Path to serve (defaults to current directory)
        path: Option<PathBuf>,
    },

    /// Show statistics about the vector database
    Stats {
        /// Path to show stats for (defaults to current directory)
        path: Option<PathBuf>,
    },

    /// Clear the vector database
    Clear {
        /// Path to clear (defaults to current directory)
        path: Option<PathBuf>,

        /// Skip confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Check installation health
    Doctor,

    /// Download embedding models
    Setup {
        /// Model to download (defaults to mxbai-embed-xsmall-v1)
        #[arg(long)]
        model: Option<String>,
    },

    /// Start MCP server for Claude Code integration
    Mcp {
        /// Path to project (defaults to current directory)
        path: Option<PathBuf>,
    },
}

pub async fn run(cancel_token: CancellationToken) -> Result<()> {
    let cli = Cli::parse();

    // Parse model from CLI flag
    let model_type = cli.model.as_ref().and_then(|m| ModelType::from_str(m));
    if cli.model.is_some() && model_type.is_none() {
        eprintln!(
            "Unknown model: '{}'. Available models:",
            cli.model.as_ref().unwrap()
        );
        eprintln!("  minilm-l6, minilm-l6-q, minilm-l12, minilm-l12-q, paraphrase-minilm");
        eprintln!("  bge-small, bge-small-q, bge-base, nomic-v1, nomic-v1.5, nomic-v1.5-q");
        eprintln!("  jina-code, e5-multilingual, mxbai-large, modernbert-large");
        std::process::exit(1);
    }

    // Set quiet mode if requested
    if cli.quiet {
        crate::output::set_quiet(true);
    }

    // Parse loglevel from CLI
    let log_level = crate::logger::LogLevel::from_str(&cli.loglevel)
        .unwrap_or(crate::logger::LogLevel::Info);

    match cli.command {
        Commands::Search {
            query,
            max_results,
            per_file,
            content,
            scores,
            compact,
            sync,
            json,
            path,
            vector_only,
            rrf_k,
            rerank,
            rerank_top,
            filter_path,
        } => {
            // Auto-enable quiet mode for JSON output
            if json {
                crate::output::set_quiet(true);
            }
            let options = SearchOptions {
                max_results,
                per_file: if per_file == 0 { None } else { Some(per_file) },
                content_lines: if content { 3 } else { 0 },
                show_scores: scores,
                compact,
                sync,
                json,
                filter_path,
                model_override: model_type.map(|mt| format!("{:?}", mt)),
                vector_only,
                rrf_k: if rrf_k == 60.0 {
                    None
                } else {
                    Some(rrf_k as usize)
                },
                rerank,
                rerank_top: if rerank_top == 50 {
                    None
                } else {
                    Some(rerank_top)
                },
            };

            crate::search::search(&query, path, options).await
        }
        Commands::Index {
            path,
            dry_run,
            force,
            add,
            global,
            remove,
            list,
        } => {
            // Check if path is "list", "add", or "rm"/"remove" as special cases (backward compatibility)
            let path_str = path.as_ref().and_then(|p| p.to_str());
            let is_list_cmd = path_str.map(|s| s == "list").unwrap_or(false);
            let is_add_cmd = path_str.map(|s| s == "add").unwrap_or(false);
            let is_rm_cmd = path_str
                .map(|s| s == "rm" || s == "remove")
                .unwrap_or(false);

            if add || is_add_cmd {
                // Clear path if it's "add" to avoid treating it as a directory
                let effective_path = if is_add_cmd { None } else { path };
                crate::index::add_to_index(effective_path, global, cancel_token.clone()).await
            } else if remove || is_rm_cmd {
                // Clear path if it's "rm"/"remove" to avoid treating it as a directory
                let effective_path = if is_rm_cmd { None } else { path };
                crate::index::remove_from_index(effective_path).await
            } else if list || is_list_cmd {
                crate::index::list_index_status().await
            } else {
                // For 'codesearch index .' or 'codesearch index <path>', just run indexing
                // The index() function will handle checking for existing indexes
                crate::index::index(path, dry_run, force, false, model_type, cancel_token.clone()).await
            }
        }
        Commands::Stats { path } => crate::index::stats(path).await,
        Commands::Serve { port, path } => {
            // Discover database path and initialize logger with file output
            // NOTE: For Serve, tracing is NOT initialized in main.rs — init_logger
            // is the first and only call to set the global subscriber
            let effective_path = path.as_ref().cloned().unwrap_or_else(|| std::env::current_dir().unwrap());
            if let Ok(Some(db_info)) = crate::db_discovery::find_best_database(Some(&effective_path)) {
                if let Err(e) = crate::logger::init_logger(&db_info.db_path, log_level, cli.quiet) {
                    eprintln!("Warning: Failed to initialize file logger: {}", e);
                }
            }
            crate::server::serve(port, path).await
        }
        Commands::Clear { path, yes } => crate::index::clear(path, yes).await,
        Commands::Doctor => crate::cli::doctor::run().await,
        Commands::Setup { model } => crate::cli::setup::run(model).await,
        Commands::Mcp { path } => {
            // Discover database path and initialize logger with file output
            // NOTE: For MCP, tracing is NOT initialized in main.rs — init_logger
            // is the first and only call to set the global subscriber
            let effective_path = path.as_ref().cloned().unwrap_or_else(|| std::env::current_dir().unwrap());
            if let Ok(Some(db_info)) = crate::db_discovery::find_best_database(Some(&effective_path)) {
                if let Err(e) = crate::logger::init_logger(&db_info.db_path, log_level, cli.quiet) {
                    eprintln!("Warning: Failed to initialize file logger: {}", e);
                }
            }
            crate::mcp::run_mcp_server(path, cancel_token).await
        }
    }
}

mod doctor;
mod setup;
