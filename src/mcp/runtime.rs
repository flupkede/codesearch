use super::proxy::{
    is_idle, mark_proxy_activity, note_connect_failure, reconnect,
    resolve_proxy_idle_disconnect_secs, McpProxyService,
};
use super::{serve_url_from_env, CodesearchService};
use crate::db_discovery::find_best_database;
use crate::embed::ModelType;
use crate::index::{IndexManager, SharedStores};
use anyhow::{Context, Result};
use rmcp::RoleClient;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

// === Server Entry Point ===

/// Run the MCP server using stdio transport with file watching for live index updates.
///
/// MCP server mode: how `codesearch mcp` connects to the index backend.
///
/// - **Auto** — If `codesearch serve` is running, connect as an HTTP client;
///   otherwise fall back to local stdio mode.
/// - **Client** — Always connect to `codesearch serve` via HTTP; fail if not running.
/// - **Local** — Always use local DB in stdio mode (classic behavior).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpMode {
    /// Connect to serve if available, otherwise local.
    #[default]
    Auto,
    /// Always connect to serve; fail if unreachable.
    Client,
    /// Always use local DB (stdio).
    Local,
}

impl std::fmt::Display for McpMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpMode::Auto => write!(f, "auto"),
            McpMode::Client => write!(f, "client"),
            McpMode::Local => write!(f, "local"),
        }
    }
}

impl std::str::FromStr for McpMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "auto" => Ok(McpMode::Auto),
            "client" => Ok(McpMode::Client),
            "local" => Ok(McpMode::Local),
            other => Err(format!(
                "invalid MCP mode '{}': must be 'auto', 'client', or 'local'",
                other
            )),
        }
    }
}

/// Probe the serve health endpoint. Returns Ok(serve_url) if serve is alive.
async fn probe_serve_health(serve_url: &str) -> bool {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(
            crate::constants::MCP_HEALTH_PROBE_TIMEOUT_MS,
        ))
        .build();
    let Ok(client) = client else { return false };
    let url = format!("{}{}", serve_url, crate::constants::HEALTH_PATH);
    client.get(&url).send().await.is_ok()
}

/// Run `codesearch mcp` as an HTTP client connecting to a running serve instance.
///
/// Uses rmcp's `StreamableHttpClientWorker` with `reqwest::Client` to speak
/// MCP Streamable HTTP to the serve hub. The MCP client (e.g. Claude Code)
/// talks JSON-RPC over stdio to us, and rmcp relays to the serve HTTP endpoint.
/// Run `codesearch mcp` as a transparent stdio↔HTTP proxy to `codesearch serve`.
///
/// Architecture:
///   Claude Desktop ──(stdio JSON-RPC)──▶ McpProxyService ──(HTTP Streamable)──▶ codesearch serve
///
/// Every MCP request from Claude Desktop is forwarded verbatim to the serve hub and the
/// response is returned unchanged. This allows Claude Desktop — which has no repo context
/// of its own — to reach all repos managed by `codesearch serve`.
///
/// ## Reconnect behaviour
///
/// When `codesearch serve` goes away (restart, crash, network blip), the proxy does NOT
/// exit. Instead it:
/// 1. Keeps the stdio connection to Claude Desktop alive
/// 2. Returns "reconnecting" errors for any incoming tool calls
/// 3. Retries the HTTP connection every 3 seconds for up to 5 minutes
/// 4. On success, hot-swaps the peer — tool calls resume immediately
/// 5. After 5 minutes of failure, exits cleanly (Claude Desktop detects the disconnect)
///
/// ## Idle disconnect behaviour
///
/// One HTTP MCP session held open for the lifetime of the proxy keeps a request
/// permanently registered at the remote's ingress, so a scale-to-zero host never
/// sees 0 concurrent requests and never suspends the replica. To avoid that, the
/// connection is only held while it is actually being used:
///
/// - An idle-checker ticks every `MCP_PROXY_IDLE_CHECK_INTERVAL_SECS`. Once no
///   request has been forwarded for `CODESEARCH_MCP_PROXY_IDLE_DISCONNECT_SECS`
///   (default `DEFAULT_MCP_PROXY_IDLE_DISCONNECT_SECS`; `0` disables and restores
///   the always-connected behaviour), it clears the peer and cancels the
///   `RunningService`, closing the transport.
/// - That is a *planned* close, not an outage: it does not open a failure window
///   and does not count against `reconnect::MAX_DURATION_SECS`. The monitor task's
///   resulting `disconnect_tx` signal is recognised (via `voluntary_disconnect`)
///   and does not trigger an eager reconnect — reconnecting immediately would
///   defeat the purpose.
/// - The next `list_tools` / `call_tool` finds an empty peer slot and signals
///   `connect_request_tx`, which reconnects on demand. Failure-path reconnects
///   are unaffected and still run on their own cadence.
async fn run_mcp_client(serve_url: &str, cancel_token: CancellationToken) -> Result<()> {
    use rmcp::{transport::stdio, ServiceExt};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let mcp_url = format!("{}{}", serve_url, crate::constants::MCP_ENDPOINT_PATH);
    tracing::info!("🔗 Connecting to codesearch serve at {}", mcp_url);

    // Channels: spawned monitor tasks notify us when their connection drops.
    let (disconnect_tx, mut disconnect_rx) = tokio::sync::mpsc::channel::<()>(1);
    let (stdio_close_tx, mut stdio_close_rx) = tokio::sync::mpsc::channel::<()>(1);
    // Capacity 1: coalescing duplicate "connect now" requests is correct.
    let (connect_request_tx, mut connect_request_rx) = tokio::sync::mpsc::channel::<()>(1);

    // Shared peer state — hot-swapped on reconnect.
    let peer_state: std::sync::Arc<tokio::sync::RwLock<Option<rmcp::service::Peer<RoleClient>>>> =
        std::sync::Arc::new(tokio::sync::RwLock::new(None));

    // Idle-disconnect state, shared with the proxy service.
    let last_activity: Arc<Mutex<std::time::Instant>> =
        Arc::new(Mutex::new(std::time::Instant::now()));
    let in_flight: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    // Notified whenever an on-demand `connect_to_serve` attempt (below, in the
    // `connect_request_rx` arm) comes back `Err` — lets `await_peer` stop waiting
    // on a definitive refusal instead of polling out the rest of its window.
    let connect_failed: Arc<tokio::sync::Notify> = Arc::new(tokio::sync::Notify::new());
    // Cancellation handle for the *current* connection. `RunningServiceCancellationToken`
    // is not Clone and its `cancel()` consumes self, so it lives in an Option slot
    // that the idle-checker `take()`s.
    let conn_cancel: Arc<
        tokio::sync::Mutex<Option<rmcp::service::RunningServiceCancellationToken>>,
    > = Arc::new(tokio::sync::Mutex::new(None));
    // Set just before we cancel a connection ourselves, so the disconnect signal it
    // produces is not mistaken for an outage.
    let voluntary_disconnect = Arc::new(AtomicBool::new(false));
    let idle_disconnect_secs = resolve_proxy_idle_disconnect_secs(None);
    if idle_disconnect_secs == 0 {
        tracing::info!("idle-disconnect disabled — holding the serve connection open");
    } else {
        tracing::info!(
            "💤 idle-disconnect enabled: closing the serve connection after {}s without traffic (checked every {}s)",
            idle_disconnect_secs,
            crate::constants::MCP_PROXY_IDLE_CHECK_INTERVAL_SECS
        );
    }

    // Step 1: Start stdio proxy for Claude Desktop.
    // This must happen first so Claude Desktop has something to talk to,
    // even before the serve connection is established.
    let proxy = McpProxyService {
        peer: peer_state.clone(),
        disconnect_tx: disconnect_tx.clone(),
        connect_request_tx: connect_request_tx.clone(),
        last_activity: last_activity.clone(),
        in_flight: in_flight.clone(),
        connect_failed: connect_failed.clone(),
    };
    let server = proxy
        .serve(stdio())
        .await
        .context("Failed to start proxy stdio server")?;

    // Spawn a task that watches the stdio connection (takes ownership of server).
    tokio::spawn(async move {
        let _ = server.waiting().await;
        let _ = stdio_close_tx.send(()).await;
    });

    // Step 2: Initial connection to serve (tolerant — may not be running yet).
    let mut serve_down_since: Option<std::time::Instant> = None;
    match connect_to_serve(
        &mcp_url,
        &peer_state,
        disconnect_tx.clone(),
        &conn_cancel,
        &last_activity,
    )
    .await
    {
        Ok(()) => {
            tracing::info!("🚀 MCP proxy ready — forwarding Claude Desktop ↔ codesearch serve");
        }
        Err(e) => {
            serve_down_since = Some(std::time::Instant::now());
            tracing::warn!(
                "codesearch serve not yet available ({}). Proxy is up, will retry every {}s.",
                e,
                reconnect::INTERVAL_SECS
            );
            // Seed a synthetic disconnect so the main loop starts reconnecting.
            let tx = disconnect_tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let _ = tx.send(()).await;
            });
        }
    }

    // Step 3: Main loop — wait for stdio close, serve disconnect, an on-demand
    // connect request, an idle timeout, or cancel.

    let mut idle_ticker = tokio::time::interval(std::time::Duration::from_secs(
        crate::constants::MCP_PROXY_IDLE_CHECK_INTERVAL_SECS,
    ));
    // The first tick of a tokio interval completes immediately; skip it so a
    // freshly started proxy is not evaluated for idleness before it can be used.
    idle_ticker.tick().await;

    loop {
        tokio::select! {
            biased; // Prefer clean shutdown paths over reconnect

            // Claude Desktop closed stdio — we're done.
            _ = stdio_close_rx.recv() => {
                tracing::info!("MCP proxy transport closed");
                return Ok(());
            }

            // External cancel signal (e.g. process termination).
            _ = cancel_token.cancelled() => {
                tracing::info!("🛑 Shutdown signal received, stopping MCP proxy...");
                return Ok(());
            }

            // A request arrived while the peer slot was empty — connect now rather
            // than waiting for the failure-path cadence. Ordered before the
            // disconnect branch so a pending 3s backoff cannot starve it.
            _ = connect_request_rx.recv() => {
                if peer_state.read().await.is_some() {
                    continue; // Someone else already reconnected.
                }
                match connect_to_serve(&mcp_url, &peer_state, disconnect_tx.clone(), &conn_cancel, &last_activity).await {
                    Ok(()) => {
                        tracing::info!("🔗 Reconnected to codesearch serve on demand");
                        serve_down_since = None;
                    }
                    Err(e) => {
                        // Serve is genuinely unreachable (or still waking). Hand over
                        // to the existing failure loop, which retries on its own
                        // cadence and eventually gives up.
                        tracing::debug!("On-demand connect failed: {}", e);
                        note_connect_failure(&connect_failed, &disconnect_tx);
                    }
                }
            }

            // Serve disconnected — enter reconnect loop.
            _ = disconnect_rx.recv() => {
                // Clear peer so tool calls get "reconnecting" error.
                {
                    let mut p = peer_state.write().await;
                    *p = None;
                }

                // A disconnect we caused on purpose (idle-close) is not an outage:
                // no failure window, no eager reconnect — the next request will ask
                // for one via connect_request_tx.
                if voluntary_disconnect.swap(false, Ordering::SeqCst) {
                    tracing::debug!(
                        "serve connection closed after idle — will reconnect on the next request"
                    );
                    continue;
                }

                if serve_down_since.is_none() {
                    serve_down_since = Some(std::time::Instant::now());
                    tracing::warn!(
                        "codesearch serve disconnected — will attempt reconnect every {}s for up to {}s",
                        reconnect::INTERVAL_SECS,
                        reconnect::MAX_DURATION_SECS,
                    );
                }

                let elapsed = serve_down_since.unwrap().elapsed();
                if elapsed.as_secs() > reconnect::MAX_DURATION_SECS {
                    tracing::error!(
                        "❌ Could not reconnect to serve after {}s — giving up",
                        reconnect::MAX_DURATION_SECS
                    );
                    return Ok(()); // Clean exit so Claude Desktop gets graceful EOF
                }

                // Wait before retrying.
                tokio::time::sleep(std::time::Duration::from_secs(reconnect::INTERVAL_SECS)).await;

                match connect_to_serve(&mcp_url, &peer_state, disconnect_tx.clone(), &conn_cancel, &last_activity).await {
                    Ok(()) => {
                        tracing::info!(
                            "✅ Reconnected to codesearch serve (was down for {:.0}s)",
                            serve_down_since.unwrap().elapsed().as_secs()
                        );
                        serve_down_since = None;
                    }
                    Err(e) => {
                        tracing::debug!("Reconnect attempt failed: {}", e);
                        // Re-trigger ourselves: the disconnect_tx from the failed
                        // connect_to_serve was never used, so we send a synthetic
                        // disconnect to keep the loop going.
                        let tx = disconnect_tx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            let _ = tx.send(()).await;
                        });
                    }
                }
            }

            // Idle check — close the connection so a scale-to-zero remote can suspend.
            _ = idle_ticker.tick() => {
                if idle_disconnect_secs == 0 {
                    continue; // Idle-disconnect disabled.
                }
                let last = match last_activity.lock() {
                    Ok(guard) => *guard,
                    Err(_) => continue, // Poisoned: never tear down on a bookkeeping error.
                };
                if !is_idle(last, idle_disconnect_secs, std::time::Instant::now()) {
                    continue;
                }

                // Take the peer slot's write lock *before* checking `in_flight`, and
                // hold it through the clear below, instead of checking `in_flight`
                // first and taking the write lock afterwards.
                //
                // Every forwarding call does `InFlightGuard::new` (increments
                // `in_flight`) BEFORE `self.peer.read().await` (list_tools/call_tool
                // above) — so a call that has already obtained `Some(peer)` to
                // forward through has necessarily already incremented the counter,
                // and a call that hasn't reached the read yet will block on it once
                // we hold the write lock. Reading `in_flight` first (the previous
                // version) left a gap between that read and taking the write lock in
                // which such a call could still slip past, get `Some(peer)`, and have
                // its transport cancelled out from under it mid-request — the
                // in-flight count and the peer-slot teardown were two separate
                // operations pretending to be one guard. See AGENTS.md
                // "counter-then-teardown races".
                let mut p = peer_state.write().await;
                if p.is_none() {
                    continue; // Already disconnected.
                }
                if in_flight.load(Ordering::SeqCst) > 0 {
                    continue; // A request is still being forwarded over this transport.
                }

                tracing::info!(
                    "💤 Idle for {}s — closing MCP proxy connection to codesearch serve (will reconnect on next request)",
                    idle_disconnect_secs
                );
                // Flag first, so the disconnect signal from the dying monitor task is
                // recognised as planned no matter how fast it arrives.
                voluntary_disconnect.store(true, Ordering::SeqCst);
                *p = None;
                drop(p);
                if let Some(token) = conn_cancel.lock().await.take() {
                    token.cancel();
                } else {
                    // Nothing to cancel — don't leave the flag set for a later,
                    // genuine disconnect to misread.
                    voluntary_disconnect.store(false, Ordering::SeqCst);
                }
            }
        }
    }
}

/// Establish (or re-establish) an HTTP MCP client connection to the serve hub.
///
/// On success, updates `peer_state` with the new peer and spawns a background task
/// that monitors the connection and sends a message on `disconnect_tx` when it drops.
///
/// Also parks the connection's cancellation handle in `conn_cancel` (so the
/// idle-checker can close it) and stamps `last_activity`, so a connection opened
/// just before an idle tick is not immediately judged idle.
async fn connect_to_serve(
    mcp_url: &str,
    peer_state: &std::sync::Arc<tokio::sync::RwLock<Option<rmcp::service::Peer<RoleClient>>>>,
    disconnect_tx: tokio::sync::mpsc::Sender<()>,
    conn_cancel: &Arc<tokio::sync::Mutex<Option<rmcp::service::RunningServiceCancellationToken>>>,
    last_activity: &Arc<Mutex<std::time::Instant>>,
) -> Result<()> {
    use rmcp::ServiceExt;

    let transport = {
        use rmcp::transport::streamable_http_client::{
            StreamableHttpClientTransportConfig, StreamableHttpClientWorker,
        };
        let config =
            StreamableHttpClientTransportConfig::with_uri(mcp_url).reinit_on_expired_session(true);
        StreamableHttpClientWorker::new(reqwest::Client::new(), config)
    };

    let http_client: rmcp::service::RunningService<RoleClient, ()> =
        ().serve(transport).await.map_err(|e| {
            anyhow::anyhow!(
                "Failed to connect to codesearch serve at {}.\n\
                 Error: {}\n\
                 Is `codesearch serve` running?",
                mcp_url,
                e
            )
        })?;

    // Grab the cancellation handle before the RunningService is moved into the
    // monitor task below — that's the only way to close this transport later
    // (idle-disconnect). Cancelling it makes the monitor's `waiting()` resolve, so
    // the normal disconnect path still runs; nothing needs special-casing.
    {
        let mut slot = conn_cancel.lock().await;
        *slot = Some(http_client.cancellation_token());
    }

    // Update the shared peer.
    let peer = http_client.peer().clone();
    {
        let mut p = peer_state.write().await;
        *p = Some(peer);
    }

    // A fresh connection counts as activity: without this, an idle tick firing
    // right after a reconnect would immediately close it again.
    mark_proxy_activity(last_activity);

    // Spawn a monitor task that detects when the connection drops.
    tokio::spawn(async move {
        let _ = http_client.waiting().await;
        // Connection lost — notify main loop.
        let _ = disconnect_tx.send(()).await;
    });

    Ok(())
}

/// # Multi-instance Support
///
/// When another instance is already running with write access to the same database,
/// this server will automatically start in **readonly mode**:
/// - Searches work normally
/// - No file watching (index won't auto-update)
/// - No incremental refresh
///
/// This allows multiple terminal windows to use codesearch simultaneously.
pub async fn run_mcp_server(
    path: Option<PathBuf>,
    create_index: bool,
    log_level: crate::logger::LogLevel,
    quiet: bool,
    mode: McpMode,
    cancel_token: CancellationToken,
) -> Result<()> {
    let serve_url = serve_url_from_env();

    // Set FASTEMBED_CACHE_DIR early (before any embedding work) to ensure fastembed
    // downloads and caches models to ~/.codesearch/models instead of creating
    // .fastembed_cache in the current working directory. Do this once for all modes.
    match crate::constants::get_global_models_cache_dir() {
        Ok(models_dir) => {
            std::env::set_var("FASTEMBED_CACHE_DIR", &models_dir);
        }
        Err(e) => {
            tracing::warn!("Could not set FASTEMBED_CACHE_DIR: {}", e);
        }
    }

    match mode {
        McpMode::Client => {
            // Client mode: init logger using global cache dir (no local DB needed)
            if let Err(e) = crate::logger::init_logger(
                &crate::constants::get_global_cache_dir(),
                log_level,
                quiet,
            ) {
                tracing::warn!("Failed to initialize file logger: {}", e);
            }
            tracing::info!("📡 MCP mode: client — connecting to serve at {}", serve_url);
            if !probe_serve_health(&serve_url).await {
                return Err(anyhow::anyhow!(
                    "codesearch serve is not running at {}. \
                     Start it with `codesearch serve` or use --mode auto/local.",
                    serve_url
                ));
            }
            return run_mcp_client(&serve_url, cancel_token).await;
        }
        McpMode::Auto => {
            // Auto mode: init logger early for probe logging
            if let Err(e) = crate::logger::init_logger(
                &crate::constants::get_global_cache_dir(),
                log_level,
                quiet,
            ) {
                tracing::warn!("Failed to initialize file logger: {}", e);
            }
            if probe_serve_health(&serve_url).await {
                tracing::info!(
                    "📡 MCP mode: auto — serve detected at {}, connecting as client",
                    serve_url
                );
                return run_mcp_client(&serve_url, cancel_token).await;
            }
            tracing::info!("📡 MCP mode: auto — no serve detected, falling back to local stdio");
            // Fall through to local mode
        }
        McpMode::Local => {
            tracing::info!("📡 MCP mode: local — using local DB (stdio)");
            // Fall through to local mode
        }
    }

    // ── Local stdio mode (original behavior) ──────────────────────────
    use rmcp::{transport::stdio, ServiceExt};

    tracing::info!("🚀 Starting codesearch MCP server");

    // Use database discovery to find the best database
    let db_info = find_best_database(path.as_deref())?;

    let (project_path, db_path) = if let Some(info) = db_info {
        (info.project_path, info.db_path)
    } else {
        // No database found
        if !create_index {
            return Err(anyhow::anyhow!(
                "No database found in current directory, parent directories, or globally tracked repositories. \
                 Run 'codesearch index' first to index the codebase, or use --create-index=true flag to automatically create it."
            ));
        }

        // Create minimal database structure to allow server to start immediately
        let effective_path = path.as_ref().cloned().unwrap_or(std::env::current_dir()?);

        // Use git root detection to place database in the correct location
        let db_root =
            crate::index::find_git_root(&effective_path)?.unwrap_or_else(|| effective_path.clone());
        let db_path = db_root.join(".codesearch.db");

        tracing::info!(
            "📁 Creating minimal database structure at {}",
            db_path.display()
        );

        // Create directory
        std::fs::create_dir_all(&db_path)?;

        // Get model info
        let model_type = ModelType::default();
        let model_short_name = model_type.short_name().to_string();
        let dimensions = model_type.dimensions();

        // Create minimal metadata.json (atomic read-modify-write, matching format
        // used by build_index). Routes through the single-source-of-truth stamp so
        // model_name matches the other index paths (previously wrote the Debug
        // variant name here, e.g. "AllMiniLML6V2Q", instead of the model name).
        crate::vectordb::merge_metadata_atomic(&db_path, |obj| {
            model_type.write_metadata_fields(obj);
            obj.insert(
                "indexed_at".to_string(),
                serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
            );
        })?;

        // Create minimal file_meta.json (matching FileMetaStore format)
        let file_meta = crate::cache::FileMetaStore::new(model_short_name.clone(), dimensions);
        file_meta.save(&db_path)?;

        // Create FTS directory
        let fts_path = db_path.join("fts");
        std::fs::create_dir_all(&fts_path)?;

        // Create LMDB file by opening VectorStore (creates minimal structure)
        let _store = crate::vectordb::VectorStore::new(&db_path, dimensions)?;

        tracing::info!("✅ Minimal database created successfully");
        tracing::info!("🔄 Background indexing will begin shortly via incremental refresh");

        (effective_path, db_path)
    };

    // Initialize file logger now that db_path is known (works for both existing and auto-created DB)
    // NOTE: For MCP, tracing is NOT initialized in main.rs — this is the only init call
    if let Err(e) = crate::logger::init_logger(&db_path, log_level, quiet) {
        tracing::warn!("Failed to initialize file logger: {}", e);
    }

    tracing::info!("📂 Project: {}", project_path.display());
    tracing::info!("💾 Database: {}", db_path.display());

    // Read model metadata to get dimensions (fallback to 384 if missing/corrupt)
    let metadata_path = db_path.join("metadata.json");
    let dimensions = if metadata_path.exists() {
        match std::fs::read_to_string(&metadata_path)
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .and_then(|j| j.get("dimensions").and_then(|v| v.as_u64()))
        {
            Some(d) => d as usize,
            None => {
                tracing::warn!(
                    "⚠️  Could not parse dimensions from metadata.json, using default {}",
                    crate::constants::DEFAULT_EMBEDDING_DIMENSIONS
                );
                crate::constants::DEFAULT_EMBEDDING_DIMENSIONS
            }
        }
    } else {
        tracing::warn!(
            "⚠️  metadata.json not found, using default dimensions {}",
            crate::constants::DEFAULT_EMBEDDING_DIMENSIONS
        );
        crate::constants::DEFAULT_EMBEDDING_DIMENSIONS
    };

    // Create shared stores - try write mode first, fall back to readonly if locked
    // This enables multiple terminal windows to use the same database
    tracing::info!("📦 Creating shared stores...");
    let (shared_stores, is_readonly) = SharedStores::new_or_readonly(&db_path, dimensions)?;
    let shared_stores = Arc::new(shared_stores);

    if is_readonly {
        tracing::warn!("🔒 Running in READONLY mode (another instance has write access)");
        tracing::warn!("   ↳ Searches work normally, but index won't auto-update");
        tracing::warn!("   ↳ Close the other instance to enable write mode");
    }

    // Create MCP service with shared stores (ready immediately)
    let service = CodesearchService::new_with_stores(
        Some(project_path.clone()),
        Some(shared_stores.clone()),
    )?;

    tracing::info!("🧠 Model: {}", service.model_type.name());

    // START MCP SERVER NOW - fixes timeout!
    tracing::info!(
        "🚀 Starting MCP server{}...",
        if is_readonly { " (readonly)" } else { "" }
    );
    let server = service.serve(stdio()).await?;

    tracing::info!("MCP server ready. Waiting for requests...");

    // Only run background tasks if we have write access
    if !is_readonly {
        // Create IndexManager with shared stores (skip initial refresh - do in background)
        tracing::info!("🔍 Initializing index manager...");
        let index_manager =
            IndexManager::new_without_refresh(&project_path, shared_stores.clone()).await?;

        // Background: refresh FIRST, then file watcher (sequential, not concurrent)
        // Both write to SharedStores, so they must not run concurrently
        let project_path_clone = project_path.clone();
        let db_path_clone = db_path.clone();
        let shared_stores_clone = shared_stores.clone();
        let index_manager_arc = Arc::new(index_manager);
        let bg_cancel_token = cancel_token.clone();
        tokio::spawn(async move {
            // Step 0: Pre-start FSW to collect file change events during refresh
            // This ensures changes made while the refresh is running are not missed
            if let Err(e) = index_manager_arc.start_watching().await {
                tracing::warn!("⚠️ Could not pre-start file watcher: {}", e);
            }

            // Step 1: Run initial refresh (writes to stores)
            tracing::info!("🔄 Starting background incremental refresh...");
            match IndexManager::perform_incremental_refresh_with_stores(
                &project_path_clone,
                &db_path_clone,
                &shared_stores_clone,
                &bg_cancel_token,
            )
            .await
            {
                Ok(_) => {
                    tracing::info!("✅ Background incremental refresh completed");

                    // Check if shutdown was requested during refresh
                    if bg_cancel_token.is_cancelled() {
                        tracing::info!("🛑 Shutdown requested, skipping file watcher startup");
                        return;
                    }

                    // Step 2: AFTER refresh completes, start file watcher (also writes to stores)
                    tracing::info!("👀 Starting file watcher...");
                    if let Err(e) = index_manager_arc
                        .start_file_watcher(bg_cancel_token, None, None)
                        .await
                    {
                        tracing::error!("❌ Failed to start file watcher: {}", e);
                    } else {
                        tracing::info!(
                            "✅ File watcher active - index will auto-update on file changes"
                        );
                    }
                }
                Err(e) => {
                    tracing::error!("❌ Background incremental refresh failed: {}", e);
                }
            }
        });

        // Start periodic log cleanup task
        let db_path_for_cleanup = db_path.clone();
        let cleanup_cancel_token = cancel_token.clone();
        tokio::spawn(async move {
            use crate::logger::{cleanup_old_logs, LogRotationConfig};

            // Run initial cleanup on startup
            let rotation_config = LogRotationConfig::from_env();
            tracing::info!("🧹 Running initial log cleanup...");
            if let Err(e) = cleanup_old_logs(&db_path_for_cleanup, &rotation_config) {
                tracing::warn!("Initial log cleanup failed: {}", e);
            }

            // Start periodic cleanup task (every 24 hours by default)
            crate::logger::start_cleanup_task(
                db_path_for_cleanup.clone(),
                rotation_config,
                cleanup_cancel_token,
            );
        });
    } else {
        tracing::info!("📖 Readonly mode: skipping background refresh and file watcher");
    }

    // Wait for shutdown: either MCP transport closes or cancellation token fires
    tokio::select! {
        result = server.waiting() => {
            tracing::info!("MCP server transport closed");
            result?;
        }
        _ = cancel_token.cancelled() => {
            tracing::info!("🛑 Shutdown signal received, stopping MCP server...");
        }
    }

    tracing::info!("✅ MCP server shut down cleanly");
    Ok(())
}
