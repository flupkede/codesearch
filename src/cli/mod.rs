use anyhow::Result;
use clap::{builder::BoolishValueParser, ArgAction, Parser, Subcommand};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

use crate::constants::{
    DEFAULT_SERVE_URL, REPO_REINDEX_PATH_PREFIX, REPO_REINDEX_PATH_SUFFIX, SERVE_PORT_ENV,
};
use crate::embed::ModelType;
use crate::search::SearchOptions;

/// Index subcommands
#[derive(Subcommand, Debug)]
pub enum IndexCommands {
    /// Add a repository to the index (creates local or global index)
    Add {
        /// Path to add (defaults to current directory).
        /// With --remote: a path on the remote peer's filesystem.
        path: Option<PathBuf>,

        /// Create global index instead of local
        #[arg(short = 'g', long)]
        global: bool,

        /// Embedding model (overrides global --model for this repo)
        #[arg(long)]
        model: Option<String>,

        /// Register the repo on a remote peer (name from `codesearch remote list`).
        /// When set, <path> is a path on the remote's filesystem.
        #[arg(long)]
        remote: Option<String>,
    },

    /// Remove the index (local or global, auto-detected)
    #[command(visible_alias = "rm")]
    Remove {
        /// Path to remove (defaults to current directory).
        /// With --remote: the remote **alias** to unregister (not a local path).
        path: Option<PathBuf>,

        /// Delete the DB only, preserve the config entry
        #[arg(long)]
        keep_config: bool,

        /// Remove a repo from a remote peer. The positional arg is a remote alias.
        #[arg(long)]
        remote: Option<String>,
    },

    /// Show index status (local, global, or on a remote peer)
    #[command(visible_alias = "ls")]
    List {
        /// List indexes on a remote peer.
        #[arg(long)]
        remote: Option<String>,

        /// Output JSON (requires --remote; agent-friendly).
        #[arg(long)]
        json: bool,
    },

    /// Rebuild symbol index (C# via scip-csharp) for a repository
    Symbol {
        /// Repository alias (required — use "index list" to see aliases)
        alias: String,

        /// Force full symbol rebuild (ignores cached state)
        #[arg(short = 'f', long)]
        force: bool,
    },

    /// Reindex a repository (incremental, or full rebuild with --force)
    Reindex {
        /// Repository alias (required — use "index list" to see aliases)
        alias: String,

        /// Force full re-index (ignore incremental state)
        #[arg(short = 'f', long)]
        force: bool,

        /// Reindex on a remote peer.
        #[arg(long)]
        remote: Option<String>,

        /// Output JSON (requires --remote; agent-friendly).
        #[arg(long)]
        json: bool,
    },

    /// Remove stale entries from repos.json (relocates moved repos first)
    Prune,
}

/// Cache subcommands
#[derive(Subcommand, Debug)]
pub enum CacheCommands {
    /// Show persistent cache statistics
    Stats {
        /// Model name (e.g., minilm-l6-q, bge-small)
        model: Option<String>,
    },

    /// Clear persistent cache
    Clear {
        /// Model name (e.g., minilm-l6-q, bge-small)
        model: Option<String>,

        /// Skip confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

/// Groups subcommands
#[derive(Subcommand, Debug)]
pub enum GroupsCommands {
    /// List all groups
    #[command(visible_alias = "ls")]
    List,

    /// Create or update a group
    Add {
        /// Group name
        name: String,

        /// Aliases to include in the group (space-separated)
        #[arg(short, long, num_args = 1..)]
        aliases: Vec<String>,
    },

    /// Remove a group
    #[command(visible_alias = "rm")]
    Remove {
        /// Group name
        name: String,
    },
}

/// Remote federation-peer subcommands
#[derive(Subcommand, Debug)]
pub enum RemoteCommands {
    /// List configured remote peers
    #[command(visible_alias = "ls")]
    List,

    /// Add (or overwrite) a remote `codesearch serve` peer for federation
    Add {
        /// Peer name (referenced from groups as "@<name>")
        name: String,

        /// Base URL of the remote serve instance (e.g. https://codesearch.example.com)
        #[arg(long, visible_alias = "base-url")]
        url: String,

        /// Bearer / X-API-Key secret accepted by the remote (required when the
        /// remote binds a non-localhost address)
        #[arg(long)]
        api_key: Option<String>,

        /// Group to query on the remote (in the remote's own repos.json);
        /// defaults to the remote's virtual "all" group when omitted
        #[arg(long)]
        group: Option<String>,

        /// Per-peer request timeout in seconds (default 15)
        #[arg(long)]
        timeout_secs: Option<u64>,

        /// Also add "@<name>" to this LOCAL group (created if needed) so the
        /// peer is actually queryable via that group
        #[arg(long)]
        into_group: Option<String>,
    },

    /// Remove a remote peer (and prune "@<name>" from any groups)
    #[command(visible_alias = "rm")]
    Remove {
        /// Peer name
        name: String,
    },

    /// List the individual projects a peer exposes, marking which are mounted
    #[command(visible_alias = "avail")]
    Available {
        /// Peer name (as configured with `remote add`)
        peer: String,
    },

    /// Mount an individual remote project locally (opt-in), by "<peer>/<alias>"
    Mount {
        /// Canonical name "<peer>/<alias>" (see `remote available`)
        name: String,
    },

    /// Unmount a previously mounted remote project, by "<peer>/<alias>"
    #[command(visible_alias = "umount")]
    Unmount {
        /// Canonical name "<peer>/<alias>"
        name: String,
    },

    /// List the remote projects currently mounted locally
    Mounts,
}

/// `hooks` subcommands — grouped by integration target.
#[derive(Subcommand, Debug)]
pub enum HookCommands {
    /// Manage the git hooks (post-checkout worktree auto-registration)
    Git {
        #[command(subcommand)]
        command: HookGitCommands,
    },
    /// Manage the Claude Code integration hooks (codesearch-first guards)
    Claude {
        #[command(subcommand)]
        command: HookClaudeCommands,
    },
}

/// `hooks git` subcommands.
#[derive(Subcommand, Debug)]
pub enum HookGitCommands {
    /// Install a post-checkout hook that auto-registers new git worktrees with codesearch serve
    Install {
        /// Path to the git repository (defaults to current directory)
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

/// `hooks claude` subcommands.
#[derive(Subcommand, Debug)]
pub enum HookClaudeCommands {
    /// Install the Claude Code PreToolUse guard hooks (codesearch-first) into settings.json
    Install {
        /// Install into the project's ./.claude instead of the user-level ~/.claude
        #[arg(long)]
        project: bool,
    },
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

/// Action for the `serve` command.
#[derive(clap::ValueEnum, Clone, Debug, Default)]
pub enum ServeAction {
    /// Start the serve process (default)
    #[default]
    Start,
    /// Open a standalone TUI connected to a running serve instance
    Tui,
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

        /// Maximum matches to show per file (0 = no limit)
        #[arg(long, default_value = "0")]
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

        /// Automatically create index if it doesn't exist (default: true)
        #[arg(
            long,
            default_value_t = true,
            action = ArgAction::Set,
            value_parser = BoolishValueParser::new(),
            num_args = 0..=1,
            require_equals = true,
            default_missing_value = "true"
        )]
        create_index: bool,
    },

    /// Index the repository or manage global index registry
    Index {
        /// Subcommand: add, rm, list (preferred)
        #[command(subcommand)]
        command: Option<IndexCommands>,

        /// Path to index (defaults to current directory) — used when no subcommand given
        path: Option<PathBuf>,

        /// Show what would be indexed without actually indexing
        #[arg(long)]
        dry_run: bool,

        /// Force full re-index
        #[arg(short = 'f', long, alias = "full")]
        force: bool,

        /// Also rebuild symbol index (C# via scip-csharp) after text reindex
        #[arg(long)]
        symbols: bool,

        // Backward-compat flags (predate subcommands)
        /// Add a repository to the index (creates local or global index)
        #[arg(long)]
        add: bool,

        /// Create global index instead of local (only with --add)
        #[arg(short = 'g', long)]
        global: bool,

        /// Remove the index (local or global, auto-detected)
        #[arg(long, visible_alias = "rm")]
        remove: bool,

        /// Delete the DB only, preserve the config entry (only with --remove)
        #[arg(long)]
        keep_config: bool,

        /// Show index status (local or global)
        #[arg(long)]
        list: bool,
    },

    /// Run a background MCP server with live file watching and multi-repo support
    Serve {
        /// Action: `start` (default) or `tui`
        #[arg(default_value = "start")]
        action: ServeAction,

        /// Host to bind to (default: 127.0.0.1, override with CODESEARCH_SERVE_HOST; use 0.0.0.0 for containers)
        #[arg(long)]
        host: Option<String>,

        /// Port to listen on (default: 39725, override with CODESEARCH_SERVE_PORT)
        #[arg(short, long)]
        port: Option<u16>,

        /// Register one or more repo paths at startup (can be repeated)
        #[arg(short, long, action = ArgAction::Append)]
        register: Vec<PathBuf>,

        /// Log to file only, not to console (use for daemon/background mode)
        #[arg(short, long, default_value = "false", action = ArgAction::Set, value_parser = BoolishValueParser::new())]
        quiet: bool,

        /// Show verbose output on console (overrides --quiet for debugging)
        #[arg(long, visible_alias = "no-quiet")]
        verbose: bool,

        /// Automatically create index if it doesn't exist (default: true)
        #[arg(
            short = 'c',
            long,
            default_value_t = true,
            action = ArgAction::Set,
            value_parser = BoolishValueParser::new(),
            num_args = 0..=1,
            require_equals = true,
            default_missing_value = "true"
        )]
        create_index: bool,

        /// Disable the embedded TUI even when a TTY is available
        #[arg(long)]
        no_tui: bool,

        /// Cloud keep-warm: self-ping this ingress URL (e.g. the app's public
        /// FQDN) to stay warm on a scale-to-zero host while recently active.
        /// Overrides CODESEARCH_KEEP_WARM_URL.
        #[arg(long)]
        keep_warm_url: Option<String>,

        /// Idle window (seconds) before keep-warm stops and the host may
        /// suspend the replica (default 7200 = 2h). Overrides
        /// CODESEARCH_IDLE_SUSPEND_SECS.
        #[arg(long)]
        idle_suspend_secs: Option<u64>,

        /// For `tui` action: serve URL to connect to
        #[arg(long, default_value = DEFAULT_SERVE_URL)]
        url: String,
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
    Doctor {
        /// Auto-repair stale/missing files by running incremental refresh
        #[arg(long)]
        fix: bool,

        /// Output as JSON for scripting/CI
        #[arg(long)]
        json: bool,

        /// Run diagnostics on all registered repositories (from repos.json)
        #[arg(long)]
        all: bool,

        /// Run diagnostics on a specific registered alias (e.g. --repo example-org)
        #[arg(long, value_name = "ALIAS")]
        repo: Option<String>,
    },

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

        /// Automatically create index if it doesn't exist (default: true)
        #[arg(
            short = 'c',
            long,
            default_value_t = true,
            action = ArgAction::Set,
            value_parser = BoolishValueParser::new(),
            num_args = 0..=1,
            require_equals = true,
            default_missing_value = "true"
        )]
        create_index: bool,

        /// MCP connection mode (default: auto, override with CODESEARCH_MCP_MODE)
        ///
        /// - auto:   Connect to serve if running, otherwise use local DB
        /// - client: Always connect to serve; fail if not running
        /// - local:  Always use local DB (classic stdio behavior)
        #[arg(short, long, env = crate::constants::MCP_MODE_ENV, default_value = "auto")]
        mode: crate::mcp::McpMode,
    },

    /// Manage repository groups
    Groups {
        #[command(subcommand)]
        command: GroupsCommands,
    },

    /// Manage remote federation peers (other `codesearch serve` instances)
    Remote {
        #[command(subcommand)]
        command: RemoteCommands,
    },

    /// Manage persistent embedding cache
    Cache {
        #[command(subcommand)]
        command: CacheCommands,
    },

    /// Manage codesearch integration hooks (git worktree auto-index, Claude Code guards)
    #[command(name = "hooks", alias = "hook")]
    Hook {
        #[command(subcommand)]
        command: HookCommands,
    },
}

// ---------------------------------------------------------------------------
// Symbol reindex via HTTP API
// ---------------------------------------------------------------------------

/// Base URL for the codesearch serve instance.
/// Override via `CODESEARCH_SERVE_HOST` and `CODESEARCH_SERVE_PORT` env vars.
fn serve_base_url() -> String {
    use crate::constants::{resolve_serve_host, DEFAULT_SERVE_PORT};
    let host = resolve_serve_host();
    let port = std::env::var(SERVE_PORT_ENV)
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(DEFAULT_SERVE_PORT);
    format!("http://{host}:{port}")
}

/// Trigger a symbol reindex by calling the running serve instance's HTTP API.
async fn trigger_symbol_reindex_via_api(alias: &str, force: bool) -> Result<()> {
    use colored::Colorize;

    let base = serve_base_url();
    let url = if force {
        format!("{base}{REPO_REINDEX_PATH_PREFIX}{alias}{REPO_REINDEX_PATH_SUFFIX}?force=true&symbols=true")
    } else {
        format!("{base}{REPO_REINDEX_PATH_PREFIX}{alias}{REPO_REINDEX_PATH_SUFFIX}?symbols=true")
    };

    println!(
        "  {} symbol reindex for '{}' via {url}",
        "⟳".yellow(),
        alias.bright_green()
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    let resp = client.post(&url).send().await;

    match resp {
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            if status.as_u16() == 202 {
                println!(
                    "  {} symbol reindex accepted — rebuilding in background",
                    "✓".green()
                );
                Ok(())
            } else if status.as_u16() == 404 {
                anyhow::bail!(
                    "Unknown alias '{}' — use `codesearch index list` to see registered repos",
                    alias
                );
            } else if status.as_u16() == 409 {
                anyhow::bail!("Reindex already in progress for '{}'", alias);
            } else {
                anyhow::bail!("Serve returned HTTP {}: {}", status.as_u16(), body.trim());
            }
        }
        Err(e) => {
            if e.is_connect() {
                anyhow::bail!(
                    "Cannot connect to codesearch serve at {}.\n  Is `codesearch serve` running?",
                    base
                );
            } else {
                anyhow::bail!("HTTP request failed: {}", e);
            }
        }
    }
}

/// Trigger a text reindex by calling the running serve instance's HTTP API.
/// Like [`trigger_symbol_reindex_via_api`] but without `symbols=true`.
async fn trigger_reindex_via_api(alias: &str, force: bool) -> Result<()> {
    use colored::Colorize;

    let base = serve_base_url();
    let url = if force {
        format!("{base}{REPO_REINDEX_PATH_PREFIX}{alias}{REPO_REINDEX_PATH_SUFFIX}?force=true")
    } else {
        format!("{base}{REPO_REINDEX_PATH_PREFIX}{alias}{REPO_REINDEX_PATH_SUFFIX}")
    };

    println!(
        "  {} reindex for '{}' via {url}",
        "⟳".yellow(),
        alias.bright_green()
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    let resp = client.post(&url).send().await;

    match resp {
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            if status.as_u16() == 202 {
                println!(
                    "  {} reindex accepted — rebuilding in background",
                    "✓".green()
                );
                Ok(())
            } else if status.as_u16() == 404 {
                anyhow::bail!(
                    "Unknown alias '{}' — use `codesearch index list` to see registered repos",
                    alias
                );
            } else if status.as_u16() == 409 {
                anyhow::bail!("Reindex already in progress for '{}'", alias);
            } else {
                anyhow::bail!("Serve returned HTTP {}: {}", status.as_u16(), body.trim());
            }
        }
        Err(e) => {
            if e.is_connect() {
                anyhow::bail!(
                    "Cannot connect to codesearch serve at {}.\n  Is `codesearch serve` running?",
                    base
                );
            } else {
                anyhow::bail!("HTTP request failed: {}", e);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Remote index management (--remote <peer>)
// ---------------------------------------------------------------------------

/// Resolve a peer name (from `codesearch remote list`) into a [`RemotePeer`].
fn resolve_remote_peer(name: &str) -> Result<crate::db_discovery::repos::RemotePeer> {
    let config = crate::db_discovery::load_repos_config()?;
    match config.remotes.get(name) {
        Some(peer) => Ok(peer.clone()),
        None => {
            let mut known: Vec<&String> = config.remotes.keys().collect();
            known.sort();
            anyhow::bail!(
                "Unknown remote '{}'.{}",
                name,
                if known.is_empty() {
                    " No remotes configured — add one with `codesearch remote add <name> --url <URL>`.".to_string()
                } else {
                    format!(
                        " Configured remotes: {}.",
                        known
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            )
        }
    }
}

/// Unwrap a [`ManagementOutcome`] into a [`Result`], producing a clear error
/// that names the peer and distinguishes "peer rejected" from "peer unreachable".
fn unwrap_management<T>(
    peer_name: &str,
    outcome: crate::federation::ManagementOutcome<T>,
) -> Result<T> {
    match outcome {
        crate::federation::ManagementOutcome::Ok(v) => Ok(v),
        crate::federation::ManagementOutcome::HttpError { status, reason } => {
            anyhow::bail!("Peer '{}' returned HTTP {}: {}", peer_name, status, reason);
        }
        crate::federation::ManagementOutcome::Unreachable(msg) => {
            anyhow::bail!("Cannot reach peer '{}': {}", peer_name, msg);
        }
    }
}

/// `codesearch index list --remote <peer>` — list repos registered on a peer.
async fn run_remote_list(peer_name: &str, json: bool) -> Result<()> {
    use colored::Colorize;

    let peer = resolve_remote_peer(peer_name)?;
    let client = crate::federation::FederationClient::new().map_err(anyhow::Error::msg)?;
    let status = unwrap_management(peer_name, client.list_repos(&peer).await)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }

    println!("Remote '{}' ({}):", peer_name.bright_cyan(), peer.url);
    if status.repos.is_empty() {
        println!("  No repositories registered on this peer.");
    } else {
        // Aligned table: alias | status | lock | changes | last_tool_call
        for repo in &status.repos {
            let last = repo.last_tool_call.as_deref().unwrap_or("—");
            println!(
                "  {:<18} {:<10} {:<6} {:>6} changes   last: {}",
                repo.alias, repo.status, repo.lock_mode, repo.changes, last
            );
        }
    }
    let mut meta = Vec::new();
    if let Some(v) = status.version {
        meta.push(format!("version: {}", v));
    }
    if let Some(u) = status.uptime_secs {
        meta.push(format!("uptime: {}", format_duration(u)));
    }
    if let Some(s) = status.active_sessions {
        meta.push(format!("sessions: {}", s));
    }
    if !meta.is_empty() {
        println!("  {}", meta.join("  |  "));
    }
    Ok(())
}

/// Format a duration in seconds as a human-readable string (e.g. "3h 24m").
fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// `codesearch index add <path> --remote <peer>` — register a repo on a peer.
async fn run_remote_add(peer_name: &str, path: Option<PathBuf>) -> Result<()> {
    use colored::Colorize;

    let remote_path = path
        .as_ref()
        .and_then(|p| p.to_str())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Path required: `codesearch index add <path> --remote <peer>` (path on the remote's filesystem)"
            )
        })?;

    let peer = resolve_remote_peer(peer_name)?;
    let client = crate::federation::FederationClient::new().map_err(anyhow::Error::msg)?;
    let added = unwrap_management(peer_name, client.add_repo(&peer, remote_path).await)?;

    println!(
        "{} Added '{}' on peer '{}' (path: {})",
        "✓".green(),
        added.alias.bright_green(),
        peer_name.bright_cyan(),
        added.path
    );
    if let Some(msg) = added.message {
        println!("  {}", msg);
    }
    Ok(())
}

/// `codesearch index rm <alias> --remote <peer>` — unregister a repo on a peer.
async fn run_remote_remove(peer_name: &str, alias: Option<PathBuf>) -> Result<()> {
    use colored::Colorize;

    let alias_str = alias
        .as_ref()
        .and_then(|p| p.to_str())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Alias required: `codesearch index rm <alias> --remote <peer>` (remote alias, not a local path)"
            )
        })?;

    let peer = resolve_remote_peer(peer_name)?;
    let client = crate::federation::FederationClient::new().map_err(anyhow::Error::msg)?;
    let removed = unwrap_management(peer_name, client.remove_repo(&peer, alias_str).await)?;

    println!(
        "{} Removed '{}' from peer '{}'",
        "✓".green(),
        removed.alias.bright_green(),
        peer_name.bright_cyan()
    );
    if let Some(msg) = removed.message {
        println!("  {}", msg);
    }
    Ok(())
}

/// `codesearch index reindex <alias> --remote <peer>` — trigger reindex on a peer.
async fn run_remote_reindex(peer_name: &str, alias: &str, force: bool, json: bool) -> Result<()> {
    use colored::Colorize;

    let peer = resolve_remote_peer(peer_name)?;
    let client = crate::federation::FederationClient::new().map_err(anyhow::Error::msg)?;
    let result = unwrap_management(peer_name, client.reindex(&peer, alias, force).await)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

    let mode = if force { "force " } else { "" };
    println!(
        "{} {}reindex started for '{}' on peer '{}'",
        "⟳".yellow(),
        mode,
        alias.bright_green(),
        peer_name.bright_cyan()
    );
    if let Some(msg) = result.message {
        println!("  {}", msg);
    }
    Ok(())
}

pub async fn run(cancel_token: CancellationToken) -> Result<()> {
    let cli = Cli::parse();

    // Parse model from CLI flag
    let model_type = cli.model.as_ref().and_then(|m| ModelType::parse(m));
    if cli.model.is_some() && model_type.is_none() {
        eprintln!(
            "Unknown model: '{}'. Available models:",
            cli.model.as_deref().unwrap_or_default()
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
    let log_level =
        crate::logger::LogLevel::parse(&cli.loglevel).unwrap_or(crate::logger::LogLevel::Info);

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
            create_index,
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
                create_index,
            };

            crate::search::search(&query, path, options).await
        }
        Commands::Index {
            command,
            path,
            dry_run,
            force,
            symbols,
            add,
            global,
            remove,
            keep_config,
            list,
        } => {
            // Subcommand path (preferred)
            if let Some(cmd) = command {
                match cmd {
                    IndexCommands::Add {
                        path: add_path,
                        global,
                        model,
                        remote,
                    } => {
                        if let Some(peer_name) = &remote {
                            run_remote_add(peer_name, add_path).await
                        } else {
                            let mt = model
                                .as_deref()
                                .and_then(|m| {
                                    let parsed = ModelType::parse(m);
                                    if parsed.is_none() {
                                        eprintln!("Unknown model: '{}'. Available models:", m);
                                        eprintln!("  {}", ModelType::valid_short_names());
                                        std::process::exit(1);
                                    }
                                    parsed
                                })
                                .or(model_type);
                            crate::index::add_to_index(add_path, global, mt, cancel_token.clone())
                                .await
                        }
                    }
                    IndexCommands::Remove {
                        path: rm_path,
                        keep_config,
                        remote,
                    } => {
                        if let Some(peer_name) = &remote {
                            run_remote_remove(peer_name, rm_path).await
                        } else {
                            crate::index::remove_from_index(rm_path, keep_config).await
                        }
                    }
                    IndexCommands::List { remote, json } => {
                        if let Some(peer_name) = &remote {
                            run_remote_list(peer_name, json).await
                        } else if json {
                            anyhow::bail!(
                                "--json is only supported with --remote (local list is always a table)"
                            )
                        } else {
                            crate::index::list_index_status().await
                        }
                    }
                    IndexCommands::Symbol { alias, force } => {
                        trigger_symbol_reindex_via_api(&alias, force).await
                    }
                    IndexCommands::Reindex {
                        alias,
                        force,
                        remote,
                        json,
                    } => {
                        if let Some(peer_name) = &remote {
                            run_remote_reindex(peer_name, &alias, force, json).await
                        } else if json {
                            anyhow::bail!(
                                "--json is only supported with --remote (local reindex prints a status line)"
                            )
                        } else {
                            trigger_reindex_via_api(&alias, force).await
                        }
                    }
                    IndexCommands::Prune => crate::index::prune_index().await,
                }
            } else {
                // Flag-based backward-compat path
                // Check if path is "list", "add", or "rm"/"remove" as special cases
                let path_str = path.as_ref().and_then(|p| p.to_str());
                let is_list_cmd = path_str.map(|s| s == "list").unwrap_or(false);
                let is_add_cmd = path_str.map(|s| s == "add").unwrap_or(false);
                let is_rm_cmd = path_str
                    .map(|s| s == "rm" || s == "remove")
                    .unwrap_or(false);

                if add || is_add_cmd {
                    let effective_path = if is_add_cmd { None } else { path };
                    crate::index::add_to_index(
                        effective_path,
                        global,
                        model_type,
                        cancel_token.clone(),
                    )
                    .await
                } else if remove || is_rm_cmd {
                    let effective_path = if is_rm_cmd { None } else { path };
                    crate::index::remove_from_index(effective_path, keep_config).await
                } else if list || is_list_cmd {
                    crate::index::list_index_status().await
                } else if symbols {
                    // --symbols without subcommand: resolve path to alias, use HTTP API
                    use crate::db_discovery::repos::ReposConfig;
                    let config = ReposConfig::load().unwrap_or_default();
                    let target_path = path.as_deref().unwrap_or_else(|| std::path::Path::new("."));
                    let resolved_alias = config.alias_for_path(target_path).or_else(|| {
                        // Try the directory name as alias fallback
                        target_path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .map(|s| s.to_string())
                    });
                    match resolved_alias {
                        Some(a) => trigger_symbol_reindex_via_api(&a, force).await,
                        None => anyhow::bail!(
                            "Cannot resolve alias for path '{}'. Use `codesearch index symbol <alias>` instead.",
                            target_path.display()
                        ),
                    }
                } else {
                    crate::index::index(
                        path,
                        dry_run,
                        force,
                        false,
                        model_type,
                        cancel_token.clone(),
                    )
                    .await
                }
            }
        }
        Commands::Stats { path } => crate::index::stats(path).await,
        Commands::Serve {
            action,
            host,
            port,
            register,
            quiet,
            verbose,
            create_index: _,
            no_tui,
            keep_warm_url,
            idle_suspend_secs,
            url,
        } => {
            match action {
                crate::cli::ServeAction::Tui => crate::serve::run_tui_standalone(url).await,
                crate::cli::ServeAction::Start => {
                    // Initialize serve logger — always logs to ~/.codesearch/logs/serve.log.YYYY-MM-DD
                    // regardless of whether a database exists in the current directory.
                    // This is a central log for the multi-repo serve process, separate from
                    // per-database logs written by the MCP client (codesearch.log.YYYY-MM-DD).
                    let effective_quiet = (cli.quiet || quiet) && !verbose;
                    if let Err(e) = crate::logger::init_serve_logger(log_level, effective_quiet) {
                        eprintln!("Warning: failed to initialize serve logger: {}", e);
                    }
                    crate::serve::run_serve(
                        host,
                        port,
                        register,
                        no_tui,
                        keep_warm_url,
                        idle_suspend_secs,
                        cancel_token.clone(),
                    )
                    .await
                }
            }
        }
        Commands::Clear { path, yes } => crate::index::clear(path, yes).await,
        Commands::Doctor {
            fix,
            json,
            all,
            repo,
        } => crate::cli::doctor::run(fix, json, all, repo).await,
        Commands::Setup { model } => crate::cli::setup::run(model).await,
        Commands::Mcp {
            path,
            create_index,
            mode,
        } => {
            // Logger is initialized inside run_mcp_server() once db_path is known.
            // This handles both the "DB already exists" and "auto-create DB" paths correctly.
            //
            // MCP stdio transport uses stdout for JSON-RPC — always force file-only
            // logging to keep the channel clean, regardless of the global --quiet flag.
            crate::mcp::run_mcp_server(path, create_index, log_level, true, mode, cancel_token)
                .await
        }
        Commands::Cache { command } => match command {
            CacheCommands::Stats { model } => run_cache_stats(model).await,
            CacheCommands::Clear { model, yes } => run_cache_clear(model, yes).await,
        },
        Commands::Groups { command } => run_groups_command(command).await,
        Commands::Remote { command } => run_remote_command(command).await,
        Commands::Hook { command } => match command {
            HookCommands::Git { command } => match command {
                HookGitCommands::Install { path } => run_hook_git_install(path).await,
            },
            HookCommands::Claude { command } => match command {
                HookClaudeCommands::Install { project } => {
                    claude_hooks::run_claude_install(project)
                }
            },
        },
    }
}

/// Show persistent cache statistics
async fn run_cache_stats(model: Option<String>) -> Result<()> {
    // Parse model name
    let model_name = model
        .as_deref()
        .map(|m| ModelType::parse(m).map(|mt| mt.short_name()))
        .ok_or_else(|| anyhow::anyhow!("Failed to parse model name"))?;

    if model_name.is_none() {
        eprintln!("Cache statistics for all models:");
    }

    // Get cache directory
    let cache_dir = crate::constants::get_global_models_cache_dir()
        .unwrap_or_default()
        .join("embedding_cache");

    if !cache_dir.exists() {
        if let Some(name) = model_name {
            eprintln!("No cache found for model: {}", name);
        } else {
            eprintln!("No cache directory found: {}", cache_dir.display());
        }
        return Ok(());
    }

    // Show stats for specific model or all models
    if let Some(name) = model_name {
        let model_cache_dir = cache_dir.join(name);
        if !model_cache_dir.exists() {
            eprintln!("No cache found for model: {}", name);
            return Ok(());
        }

        let cache = crate::embed::PersistentEmbeddingCache::open(name)?;
        let stats = cache.stats()?;

        println!("Persistent Cache Statistics ({})", name);
        println!("  Cache Directory: {}", model_cache_dir.display());
        println!("  Total Entries: {}", stats.entries);
        println!("  Database Size: {} bytes", stats.file_size_bytes);
        println!(
            "    Last Access: {}",
            stats
                .last_access
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_else(|| "N/A".to_string())
        );
    } else {
        // Show stats for all models
        let dir_entries = std::fs::read_dir(&cache_dir)?;
        let mut model_count = 0;
        let mut total_size = 0;

        println!("Persistent Cache Statistics (All Models)");
        for entry in dir_entries {
            let entry = entry?;
            if entry.path().is_dir() {
                let model_name = entry.file_name().to_string_lossy().to_string();
                let cache = crate::embed::PersistentEmbeddingCache::open(&model_name)?;
                let stats = cache.stats()?;
                model_count += stats.entries;
                total_size += stats.file_size_bytes;

                println!("  {}:", model_name);
                println!("    Entries: {}", stats.entries);
                println!("    Size: {} bytes", stats.file_size_bytes);
                println!(
                    "    Last Access: {}",
                    stats
                        .last_access
                        .map(|dt| dt.to_rfc3339())
                        .unwrap_or_else(|| "N/A".to_string())
                );
            }
        }
        println!("Total: {} models, {} bytes", model_count, total_size);
    }

    Ok(())
}

/// Clear persistent cache
async fn run_cache_clear(model: Option<String>, yes: bool) -> Result<()> {
    // Parse model name
    let model_name = model
        .as_deref()
        .map(|m| ModelType::parse(m).map(|mt| mt.short_name()))
        .ok_or_else(|| anyhow::anyhow!("Failed to parse model name"))?;

    // Get cache directory
    let cache_dir = crate::constants::get_global_models_cache_dir()
        .unwrap_or_default()
        .join("embedding_cache");

    if !cache_dir.exists() {
        eprintln!("No cache directory found: {}", cache_dir.display());
        return Ok(());
    }

    // Confirm unless --yes flag is set
    if !yes {
        if let Some(name) = &model_name {
            eprint!(
                "Are you sure you want to clear the cache for model '{}'? [y/N]: ",
                name
            );
        } else {
            eprint!("Are you sure you want to clear the cache for ALL models? [y/N]: ");
        }
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().to_lowercase().starts_with('y') {
            eprintln!("Aborted.");
            return Ok(());
        }
    }

    // Clear cache for specific model or all models
    if let Some(name) = model_name {
        let model_cache_dir = cache_dir.join(name);
        if !model_cache_dir.exists() {
            eprintln!("No cache found for model: {}", name);
            return Ok(());
        }

        let cache = crate::embed::PersistentEmbeddingCache::open(name)?;
        let stats_before = cache.stats()?;
        cache.clear()?;
        eprintln!(
            "Cleared {} entries from cache for model '{}'",
            stats_before.entries, name
        );
    } else {
        // Clear all caches
        let entries = std::fs::read_dir(&cache_dir)?;
        let mut total_cleared = 0;

        for entry in entries {
            let entry = entry?;
            if entry.path().is_dir() {
                let model_name = entry.file_name().to_string_lossy().to_string();
                let cache = crate::embed::PersistentEmbeddingCache::open(&model_name)?;
                let stats_before = cache.stats()?;
                cache.clear()?;
                total_cleared += stats_before.entries;
                eprintln!(
                    "Cleared {} entries from cache for model '{}'",
                    stats_before.entries, model_name
                );
            }
        }
        eprintln!("Total: {} entries cleared", total_cleared);
    }

    Ok(())
}

/// Handle groups subcommands
async fn run_groups_command(command: GroupsCommands) -> Result<()> {
    use crate::constants::ALL_GROUP_NAME;
    match command {
        GroupsCommands::List => {
            let config = crate::db_discovery::load_repos_config()?;
            // The virtual "all" group is always present when repos are registered.
            let has_all = !config.repos.is_empty();
            if config.groups.is_empty() && !has_all {
                println!("No groups defined.");
                return Ok(());
            }
            println!("Groups:");
            if has_all {
                let count = config.repos.len();
                println!("  {} (virtual): {} repos", ALL_GROUP_NAME, count);
            }
            for (name, aliases) in &config.groups {
                println!("  {}: {}", name, aliases.join(", "));
            }
        }
        GroupsCommands::Add { name, aliases } => {
            if name == ALL_GROUP_NAME {
                return Err(anyhow::anyhow!(
                    "Group name '{}' is reserved — it always resolves to all registered repos automatically.",
                    name
                ));
            }
            if aliases.is_empty() {
                return Err(anyhow::anyhow!(
                    "--aliases is required and must specify at least one alias."
                ));
            }
            let mut config = crate::db_discovery::load_repos_config()?;
            // Validate that all aliases exist
            for alias in &aliases {
                if !config.repos.contains_key(alias) {
                    return Err(anyhow::anyhow!(
                        "alias '{}' is not registered. Use 'codesearch index add' first.",
                        alias
                    ));
                }
            }
            config.add_group(name.clone(), aliases)?;
            config.save()?;
            println!("Group '{}' created/updated.", name);
        }
        GroupsCommands::Remove { name } => {
            if name == ALL_GROUP_NAME {
                return Err(anyhow::anyhow!(
                    "Group '{}' is reserved and cannot be removed.",
                    name
                ));
            }
            let mut config = crate::db_discovery::load_repos_config()?;
            if config.remove_group(&name) {
                config.save()?;
                println!("Group '{}' removed.", name);
            } else {
                eprintln!("Group '{}' not found.", name);
            }
        }
    }
    Ok(())
}

/// Handle remote federation-peer subcommands
async fn run_remote_command(command: RemoteCommands) -> Result<()> {
    use crate::constants::DEFAULT_REMOTE_TIMEOUT_SECS;
    use crate::db_discovery::repos::RemotePeer;

    match command {
        RemoteCommands::List => {
            let config = crate::db_discovery::load_repos_config()?;
            if config.remotes.is_empty() {
                println!("No remote peers configured.");
                return Ok(());
            }
            println!("Remote peers:");
            let mut names: Vec<&String> = config.remotes.keys().collect();
            names.sort();
            for name in names {
                let peer = &config.remotes[name];
                let group = peer.group.as_deref().unwrap_or("(remote's \"all\")");
                let timeout = peer.timeout_secs.unwrap_or(DEFAULT_REMOTE_TIMEOUT_SECS);
                let auth = if peer.api_key.is_empty() {
                    "no api-key"
                } else {
                    "api-key set"
                };
                let refs = config.groups_referencing_remote(name);
                let wired = if refs.is_empty() {
                    "not in any group — add with --into-group or `codesearch groups`".to_string()
                } else {
                    format!("groups: {}", refs.join(", "))
                };
                println!(
                    "  @{name}: {url} [remote-group={group}, timeout={timeout}s, {auth}]\n      {wired}",
                    url = peer.url
                );
            }
        }
        RemoteCommands::Add {
            name,
            url,
            api_key,
            group,
            timeout_secs,
            into_group,
        } => {
            let mut config = crate::db_discovery::load_repos_config()?;
            let peer = RemotePeer {
                url,
                api_key: api_key.unwrap_or_default(),
                group,
                timeout_secs,
            };
            config.add_remote(name.clone(), peer)?;
            if let Some(g) = &into_group {
                config.add_remote_to_group(g.clone(), name.trim())?;
            }
            config.save()?;
            println!("Remote peer '{}' added/updated.", name.trim());
            if let Some(g) = into_group {
                println!("  wired into group '{}' as \"@{}\".", g, name.trim());
            } else {
                println!(
                    "  note: add it to a group to query it, e.g. `codesearch remote add {} --url ... --into-group docs`",
                    name.trim()
                );
            }
        }
        RemoteCommands::Remove { name } => {
            let name = name.trim();
            let mut config = crate::db_discovery::load_repos_config()?;
            if config.remove_remote(name) {
                config.save()?;
                println!("Remote peer '{}' removed.", name);
            } else {
                eprintln!("Remote peer '{}' not found.", name);
            }
        }
        RemoteCommands::Available { peer } => {
            use crate::db_discovery::repos::remote_project_name;
            use crate::federation::{FederationClient, ManagementOutcome};

            let peer_name = peer.trim();
            let config = crate::db_discovery::load_repos_config()?;
            let Some(peer_cfg) = config.remotes.get(peer_name) else {
                anyhow::bail!(
                    "Unknown remote peer '{}'. Add it first with `codesearch remote add`.",
                    peer_name
                );
            };
            let client = FederationClient::new()
                .map_err(|e| anyhow::anyhow!("failed to init HTTP client: {e}"))?;
            match client.list_repos(peer_cfg).await {
                ManagementOutcome::Ok(status) => {
                    if status.repos.is_empty() {
                        println!("Peer '{}' exposes no projects.", peer_name);
                        return Ok(());
                    }
                    let mounted: std::collections::HashSet<&String> =
                        config.remote_mounts.iter().collect();
                    let mut repos = status.repos;
                    repos.sort_by(|a, b| a.alias.cmp(&b.alias));
                    println!("Projects on '{}':", peer_name);
                    for r in &repos {
                        let canonical = remote_project_name(peer_name, &r.alias);
                        let mark = if mounted.contains(&canonical) {
                            "✓ mounted"
                        } else {
                            "  -      "
                        };
                        println!("  {mark}  {canonical}  [{}]", r.status);
                    }
                    println!("\nMount one with: codesearch remote mount <peer>/<alias>");
                }
                ManagementOutcome::HttpError { status, reason } => {
                    anyhow::bail!("Peer '{}' returned HTTP {}: {}", peer_name, status, reason);
                }
                ManagementOutcome::Unreachable(reason) => {
                    anyhow::bail!("Peer '{}' unreachable: {}", peer_name, reason);
                }
            }
        }
        RemoteCommands::Mount { name } => {
            let name = name.trim();
            let mut config = crate::db_discovery::load_repos_config()?;
            config.mount_remote_project(name)?;
            config.save()?;
            println!(
                "Mounted remote project '{}'. Query it with `project={}`.",
                name, name
            );
            println!("  (If `codesearch serve` is running, press 'l' in its TUI to reload.)");
        }
        RemoteCommands::Unmount { name } => {
            let name = name.trim();
            let mut config = crate::db_discovery::load_repos_config()?;
            if config.unmount_remote_project(name) {
                config.save()?;
                println!("Unmounted remote project '{}'.", name);
            } else {
                eprintln!("Remote project '{}' was not mounted.", name);
            }
        }
        RemoteCommands::Mounts => {
            use crate::db_discovery::repos::{remote_project_name, Target};

            let config = crate::db_discovery::load_repos_config()?;
            if config.remote_mounts.is_empty() {
                println!("No remote projects mounted. See `codesearch remote available <peer>`.");
                return Ok(());
            }
            println!("Mounted remote projects:");
            for (name, target) in config.mounted_remote_projects() {
                if let Target::RemoteProject {
                    peer_name,
                    peer,
                    remote_alias,
                } = target
                {
                    let canonical = remote_project_name(&peer_name, &remote_alias);
                    if name == canonical {
                        println!("  {name}  ({})", peer.url);
                    } else {
                        // A local rename override is in effect.
                        println!("  {name}  → {canonical}  ({})", peer.url);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Install the post-checkout git hook for codesearch worktree auto-indexing
/// (`codesearch hooks git install`).
async fn run_hook_git_install(path: Option<PathBuf>) -> Result<()> {
    use colored::Colorize;

    let repo_path = path.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let dot_git = repo_path.join(".git");

    // Resolve the actual .git directory (handle worktrees where .git is a file)
    let git_dir = if dot_git.is_file() {
        // Worktree: read "gitdir: <path>" from .git file
        let content = std::fs::read_to_string(&dot_git)?;
        let first_line = content.lines().next().unwrap_or("");
        if let Some(rel) = first_line.strip_prefix("gitdir: ") {
            let resolved = repo_path.join(rel.trim());
            if resolved.exists() {
                resolved
            } else {
                anyhow::bail!("Could not resolve git dir from worktree .git file");
            }
        } else {
            anyhow::bail!("Unexpected .git file format (expected 'gitdir: ...')");
        }
    } else if dot_git.is_dir() {
        dot_git
    } else {
        anyhow::bail!("Not a git repository: {}", repo_path.display());
    };

    let hooks_dir = git_dir.join("hooks");
    std::fs::create_dir_all(&hooks_dir)?;

    let hook_path = hooks_dir.join("post-checkout");

    if hook_path.exists() {
        // Check if it's already our hook
        let existing = std::fs::read_to_string(&hook_path)?;
        if existing.contains("codesearch post-checkout hook") {
            eprintln!(
                "{}",
                "✓ codesearch post-checkout hook already installed.".green()
            );
            return Ok(());
        }
        anyhow::bail!(
            "A post-checkout hook already exists at {}.\nRemove it first or merge manually.",
            hook_path.display()
        );
    }

    // Hook script: bash (works in Git Bash on Windows too)
    let hook_script = r#"#!/bin/bash
# codesearch post-checkout hook
# Auto-registers new worktrees with codesearch serve.
# Installed by: codesearch hooks git install
# $1 = prev_ref, $2 = new_ref, $3 = flag (1=branch checkout)

SERVE_URL_FILE="$HOME/.codesearch/serve_url"
if [ -f "$SERVE_URL_FILE" ]; then
    SERVE_URL=$(cat "$SERVE_URL_FILE")
    if [ -n "$SERVE_URL" ]; then
        # JSON-escape the repo path before embedding it in the request body.
        # A path containing a double quote or backslash would otherwise break
        # out of the JSON string literal (malformed body / injection). Use
        # quoted variables as the search/replace operands so the patterns match
        # LITERALLY — bare backslash patterns (${v//\\/..}) are unreliable across
        # bash/msys builds. Escape backslashes first, then double quotes.
        REPO_PATH="$(pwd)"
        BS='\'
        DQ='"'
        REPO_PATH=${REPO_PATH//"$BS"/"$BS$BS"}
        REPO_PATH=${REPO_PATH//"$DQ"/"$BS$DQ"}
        curl -s -X POST "$SERVE_URL/repos" \
            -H "Content-Type: application/json" \
            -d "{\"path\":\"$REPO_PATH\"}" &>/dev/null &
    fi
fi
"#;

    std::fs::write(&hook_path, hook_script)?;

    // Make executable (on Unix; on Windows Git Bash this is a no-op but harmless)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&hook_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook_path, perms)?;
    }

    eprintln!(
        "{}",
        format!("✓ Installed post-checkout hook at {}", hook_path.display()).green()
    );
    eprintln!("  New worktrees will be auto-registered with codesearch serve.");

    Ok(())
}

pub mod claude_hooks;
pub mod doctor;
pub mod setup;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mcp_create_index_defaults_to_true() {
        let cli = Cli::try_parse_from(["codesearch", "mcp"]).expect("cli parse should succeed");
        match cli.command {
            Commands::Mcp { create_index, .. } => assert!(create_index),
            _ => panic!("expected Mcp command"),
        }
    }

    #[test]
    fn test_mcp_create_index_false_via_equals() {
        let cli = Cli::try_parse_from(["codesearch", "mcp", "--create-index=false"])
            .expect("cli parse should succeed");
        match cli.command {
            Commands::Mcp { create_index, .. } => assert!(!create_index),
            _ => panic!("expected Mcp command"),
        }
    }

    #[test]
    fn test_mcp_create_index_true_via_flag() {
        let cli = Cli::try_parse_from(["codesearch", "mcp", "--create-index"])
            .expect("cli parse should succeed");
        match cli.command {
            Commands::Mcp { create_index, .. } => assert!(create_index),
            _ => panic!("expected Mcp command"),
        }
    }

    #[test]
    fn test_cli_no_repos_subcommand() {
        let result = Cli::try_parse_from(["codesearch", "repos", "--help"]);
        assert!(result.is_err(), "'repos' subcommand should no longer exist");
    }

    #[test]
    fn test_cli_index_add_rejects_alias_flag() {
        // The user-settable alias was removed; the flag must no longer parse.
        let result = Cli::try_parse_from([
            "codesearch",
            "index",
            "add",
            "/tmp/foo",
            "--alias",
            "myrepo",
        ]);
        assert!(
            result.is_err(),
            "'--alias' flag should no longer be accepted on `index add`"
        );
    }

    #[test]
    fn test_cli_index_add_parses_without_alias() {
        let cli = Cli::try_parse_from(["codesearch", "index", "add", "/tmp/foo"])
            .expect("cli parse should succeed");
        match cli.command {
            Commands::Index {
                command: Some(IndexCommands::Add { path: Some(p), .. }),
                ..
            } => assert_eq!(p, std::path::PathBuf::from("/tmp/foo")),
            _ => panic!("expected Index::Add subcommand"),
        }
    }

    #[test]
    fn test_cli_index_rm_accepts_keep_config_flag() {
        let cli = Cli::try_parse_from(["codesearch", "index", "rm", "/tmp/foo", "--keep-config"])
            .expect("cli parse should succeed");
        match cli.command {
            Commands::Index {
                command:
                    Some(IndexCommands::Remove {
                        keep_config: true, ..
                    }),
                ..
            } => (),
            _ => panic!("expected Index::Remove subcommand with keep_config"),
        }
    }

    // --- --remote flag tests ---

    #[test]
    fn test_cli_index_add_with_remote() {
        let cli = Cli::try_parse_from([
            "codesearch",
            "index",
            "add",
            "/app/docs",
            "--remote",
            "peer-a",
        ])
        .expect("cli parse should succeed");
        match cli.command {
            Commands::Index {
                command:
                    Some(IndexCommands::Add {
                        path,
                        remote: Some(peer),
                        ..
                    }),
                ..
            } => {
                assert_eq!(path.as_deref(), Some(std::path::Path::new("/app/docs")));
                assert_eq!(peer, "peer-a");
            }
            _ => panic!("expected Index::Add with --remote"),
        }
    }

    #[test]
    fn test_cli_index_rm_with_remote() {
        let cli =
            Cli::try_parse_from(["codesearch", "index", "rm", "inriver", "--remote", "peer-a"])
                .expect("cli parse should succeed");
        match cli.command {
            Commands::Index {
                command:
                    Some(IndexCommands::Remove {
                        remote: Some(peer), ..
                    }),
                ..
            } => assert_eq!(peer, "peer-a"),
            _ => panic!("expected Index::Remove with --remote"),
        }
    }

    #[test]
    fn test_cli_index_list_with_remote() {
        let cli = Cli::try_parse_from([
            "codesearch",
            "index",
            "list",
            "--remote",
            "peer-a",
            "--json",
        ])
        .expect("cli parse should succeed");
        match cli.command {
            Commands::Index {
                command:
                    Some(IndexCommands::List {
                        remote: Some(peer),
                        json: true,
                    }),
                ..
            } => assert_eq!(peer, "peer-a"),
            _ => panic!("expected Index::List with --remote and --json"),
        }
    }

    #[test]
    fn test_cli_index_reindex_local() {
        let cli = Cli::try_parse_from(["codesearch", "index", "reindex", "docs"])
            .expect("cli parse should succeed");
        match cli.command {
            Commands::Index {
                command:
                    Some(IndexCommands::Reindex {
                        alias,
                        force: false,
                        remote: None,
                        json: false,
                    }),
                ..
            } => assert_eq!(alias, "docs"),
            _ => panic!("expected Index::Reindex (local)"),
        }
    }

    #[test]
    fn test_cli_index_reindex_with_remote_and_force() {
        let cli = Cli::try_parse_from([
            "codesearch",
            "index",
            "reindex",
            "inriver",
            "--force",
            "--remote",
            "peer-a",
        ])
        .expect("cli parse should succeed");
        match cli.command {
            Commands::Index {
                command:
                    Some(IndexCommands::Reindex {
                        alias,
                        force: true,
                        remote: Some(peer),
                        ..
                    }),
                ..
            } => {
                assert_eq!(alias, "inriver");
                assert_eq!(peer, "peer-a");
            }
            _ => panic!("expected Index::Reindex with --remote and --force"),
        }
    }
}
