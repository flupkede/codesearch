//! `codesearch serve` — MCP streamable HTTP server mode.
//!
//! Binds on `127.0.0.1:{port}` and serves:
//! - `GET /health` → JSON health check
//! - MCP streamable HTTP at `/mcp` via rmcp tower service
//!
//! Holds a `DashMap<String, Arc<SharedStores>>` keyed by repo alias.
//! Lazy-opens stores on first query. Conflicted repos are isolated.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::response::Json as AxumJson;
use dashmap::DashMap;
use rmcp::transport::{
    StreamableHttpServerConfig, StreamableHttpService,
    streamable_http_server::session::local::LocalSessionManager,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::constants::{
    DEFAULT_SERVE_PORT, HEALTH_PATH, MCP_ENDPOINT_PATH, SERVE_PORT_ENV, DB_DIR_NAME,
};
use crate::db_discovery::repos::ReposConfig;
use crate::index::SharedStores;
use crate::mcp::types::HealthResponse;

/// Per-repo state managed by the serve instance.
pub(crate) enum RepoState {
    /// Successfully opened with write lock.
    Open { stores: Arc<SharedStores> },
    /// Write-lock acquisition failed — queries for this repo return conflict error.
    Conflicted,
}

impl std::fmt::Debug for RepoState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepoState::Open { .. } => f.debug_struct("RepoState::Open").finish(),
            RepoState::Conflicted => f.debug_struct("RepoState::Conflicted").finish(),
        }
    }
}

/// Shared state for the serve mode.
pub(crate) struct ServeState {
    /// Repo alias → opened stores (or conflicted marker).
    repos: DashMap<String, RepoState>,
    /// Loaded repos config (alias → path).
    config: ReposConfig,
}

impl std::fmt::Debug for ServeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeState")
            .field("repo_count", &self.repos.len())
            .field("config_repos", &self.config.repos.len())
            .finish()
    }
}

impl ServeState {
    fn new(config: ReposConfig) -> Self {
        Self {
            repos: DashMap::new(),
            config,
        }
    }

    /// Try to open a repo by alias. Returns a clone of the Arc<SharedStores>
    /// if successful, or an error string if conflicted/unknown.
    pub(crate) fn get_or_open_stores(
        &self,
        alias: &str,
    ) -> std::result::Result<Arc<SharedStores>, String> {
        // Fast path: already opened
        if let Some(entry) = self.repos.get(alias) {
            return match entry.value() {
                RepoState::Open { stores } => Ok(stores.clone()),
                RepoState::Conflicted => Err(format!(
                    "Repo '{}' is currently locked by another codesearch process. \
                     Stop that process and restart serve, or use the standalone MCP for that repo.",
                    alias
                )),
            };
        }

        // Slow path: need to open
        let path = self
            .config
            .resolve(alias)
            .ok_or_else(|| format!("Unknown alias '{}'", alias))?;

        let db_path = path.join(DB_DIR_NAME);

        // Read dimensions from metadata
        let dims = self.get_dimensions_for_path(&db_path);

        // Try write-mode first, then readonly
        let stores = match SharedStores::new(&db_path, dims) {
            Ok(s) => {
                info!("Opened repo '{}' in write mode", alias);
                s
            }
            Err(_) => {
                // Try readonly
                match SharedStores::new_readonly(&db_path, dims) {
                    Ok(s) => {
                        info!("Opened repo '{}' in readonly mode", alias);
                        s
                    }
                    Err(e) => {
                        warn!("Failed to open repo '{}': {}", alias, e);
                        self.repos
                            .insert(alias.to_string(), RepoState::Conflicted);
                        return Err(format!(
                            "Repo '{}' is currently locked by another codesearch process. \
                             Stop that process and restart serve, or use the standalone MCP for that repo.",
                            alias
                        ));
                    }
                }
            }
        };

        let stores = Arc::new(stores);
        self.repos
            .insert(alias.to_string(), RepoState::Open {
                stores: stores.clone(),
            });
        Ok(stores)
    }

    fn get_dimensions_for_path(&self, db_path: &std::path::Path) -> usize {
        let metadata_path = db_path.join("metadata.json");
        if let Ok(content) = std::fs::read_to_string(&metadata_path) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(dims) = json.get("dimensions").and_then(|v| v.as_u64()) {
                    return dims as usize;
                }
            }
        }
        crate::constants::DEFAULT_EMBEDDING_DIMENSIONS // default
    }

    /// Get all registered aliases.
    pub(crate) fn aliases(&self) -> Vec<String> {
        self.config.repos.keys().cloned().collect()
    }

    /// Resolve a group name to its constituent aliases.
    /// Returns an error if the group doesn't exist.
    pub(crate) fn resolve_group_aliases(&self, group: &str) -> std::result::Result<Vec<String>, String> {
        self.config.groups.get(group)
            .cloned()
            .ok_or_else(|| format!("Unknown group '{}'", group))
    }
}

/// Health check handler: GET /health
async fn health_handler() -> AxumJson<serde_json::Value> {
    AxumJson(json!(HealthResponse {
        codesearch_server: true,
        version: env!("CARGO_PKG_VERSION").to_string(),
    }))
}

/// Run the MCP serve mode.
///
/// This is the entry point called from CLI when `codesearch serve` is invoked.
pub async fn run_serve(
    port: Option<u16>,
    register_paths: Vec<PathBuf>,
    cancel_token: CancellationToken,
) -> Result<()> {
    let effective_port = port.unwrap_or_else(|| {
        std::env::var(SERVE_PORT_ENV)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_SERVE_PORT)
    });

    // Load repos config (register any --register paths first)
    let mut config = ReposConfig::load().unwrap_or_default();
    for path in &register_paths {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        let alias = config.register(canonical);
        info!("Registered repo '{}' -> {}", alias, path.display());
    }
    if !register_paths.is_empty() {
        config.save().context("Failed to save repos config")?;
    }

    let serve_state = Arc::new(ServeState::new(config));

    // Log startup
    let addr = SocketAddr::from(([127, 0, 0, 1], effective_port));
    info!("🚀 Starting codesearch serve on {}", addr);
    info!("📋 Registered repos: {:?}", serve_state.aliases());

    // Create the MCP service factory — each session gets a fresh CodesearchService
    // that uses serve_state for repo routing.
    let state_for_factory = serve_state.clone();
    let service_factory = move || -> std::result::Result<crate::mcp::CodesearchService, std::io::Error> {
        // We create a minimal service; actual repo routing is handled inside
        // the tool handlers via serve_state.
        crate::mcp::CodesearchService::new_for_serve(state_for_factory.clone())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    };

    let session_manager = Arc::new(LocalSessionManager::default());
    let config = StreamableHttpServerConfig::default();

    let mcp_service = StreamableHttpService::new(
        service_factory,
        session_manager,
        config,
    );

    // Build axum router
    let app = axum::Router::new()
        .route(HEALTH_PATH, axum::routing::get(health_handler))
        .nest_service(MCP_ENDPOINT_PATH, mcp_service);

    // Graceful shutdown
    let server = axum::serve(
        tokio::net::TcpListener::bind(addr).await?,
        app,
    )
    .with_graceful_shutdown(async move {
        cancel_token.cancelled().await;
        info!("🛑 codesearch serve shutting down...");
    });

    info!("✅ codesearch serve ready at http://{}", addr);
    info!("   Health: http://{}{}", addr, HEALTH_PATH);
    info!("   MCP:    http://{}{}", addr, MCP_ENDPOINT_PATH);

    server.await.context("Serve error")?;

    info!("✅ codesearch serve shut down cleanly");
    Ok(())
}
