//! MCP stdio→HTTP proxy used when `codesearch serve` is detected.
//!
//! When `codesearch mcp` starts and detects a running serve instance (via
//! `/health` probe), it enters proxy mode: no local DB is opened, and all
//! MCP tool calls are forwarded to the serve instance over streamable HTTP.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use rmcp::model::{CallToolResult, JsonObject};
use serde_json::Value;

use crate::constants::{DEFAULT_SERVE_PORT, HEALTH_PATH, HEALTH_PROBE_TIMEOUT_MS, SERVE_PORT_ENV};

/// Fixed error message returned when the serve instance becomes unreachable
/// mid-session. Exact wording specified by AGENTS_multi_repo.md.
const DEAD_SESSION_MSG: &str =
    "codesearch serve is no longer reachable at {}. This MCP session cannot \
     recover. Restart the MCP client to reconnect, or restart `codesearch serve` first.";

/// Format the dead-session message with the given URL.
fn dead_session_text(url: &str) -> String {
    DEAD_SESSION_MSG.replace("{}", url)
}

/// Resolve the serve base URL from env or default port.
pub fn serve_url_from_env() -> String {
    let port = std::env::var(SERVE_PORT_ENV)
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(DEFAULT_SERVE_PORT);
    format!("http://127.0.0.1:{}", port)
}

/// Health-check response expected from the serve instance.
#[derive(serde::Deserialize)]
struct HealthBody {
    codesearch_server: bool,
    version: String,
}

/// MCP proxy that forwards tool calls to a running `codesearch serve`.
pub struct McpProxy {
    base_url: String,
    client: reqwest::Client,
    /// Once set to true, all subsequent calls return the fixed dead-session message.
    dead: AtomicBool,
}

impl std::fmt::Debug for McpProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpProxy")
            .field("base_url", &self.base_url)
            .field("dead", &self.dead.load(Ordering::SeqCst))
            .finish()
    }
}

impl McpProxy {
    pub fn new(base_url: String) -> Self {
        Self {
            base_url,
            client: reqwest::Client::new(),
            dead: AtomicBool::new(false),
        }
    }

    /// Probe the serve `/health` endpoint.
    ///
    /// Returns:
    /// - `Ok(true)` — serve is reachable, version matches
    /// - `Ok(false)` — serve not reachable / bad response (fall through to stdio)
    /// - `Err` — version mismatch (caller should hard-error)
    pub async fn check_health(base_url: &str) -> Result<bool> {
        let url = format!("{}{}", base_url, HEALTH_PATH);
        let client = reqwest::Client::new();

        let resp = match tokio::time::timeout(
            std::time::Duration::from_millis(HEALTH_PROBE_TIMEOUT_MS),
            client.get(&url).send(),
        )
        .await
        {
            Ok(Ok(r)) => r,
            _ => return Ok(false), // Not reachable — fall through to stdio
        };

        let body: HealthBody = match resp.json().await {
            Ok(b) => b,
            _ => return Ok(false), // Bad JSON — fall through
        };

        if !body.codesearch_server {
            return Ok(false);
        }

        // Version check
        let my_version = env!("CARGO_PKG_VERSION").to_string();
        if body.version != my_version {
            return Err(anyhow::anyhow!(
                "codesearch serve version mismatch: serve={} mcp={}. \
                 Restart serve or update the mcp binary.",
                body.version,
                my_version
            ));
        }

        Ok(true)
    }

    /// Forward a tool call to the serve instance.
    ///
    /// Sets `dead=true` on any connection error, after which all calls return
    /// the fixed error message.
    pub async fn forward(
        &self,
        tool: &str,
        params: Option<Value>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if self.dead.load(Ordering::SeqCst) {
            return Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                dead_session_text(&self.base_url),
            )]));
        }

        // Build the JSON-RPC request body matching MCP spec
        let mut args_map: JsonObject = serde_json::Map::new();
        if let Some(p) = params {
            if let Value::Object(map) = p {
                args_map = map;
            }
        }

        let request_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": tool,
                "arguments": args_map,
            }
        });

        let url = format!("{}/mcp", self.base_url);

        let resp = match self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .json(&request_body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(_) => {
                self.dead.store(true, Ordering::SeqCst);
                return Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                    dead_session_text(&self.base_url),
                )]));
            }
        };

        // Parse the response — could be JSON or SSE
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if content_type.contains("text/event-stream") {
            // SSE response — read the body as text and extract the data
            let body = match resp.text().await {
                Ok(b) => b,
                Err(_) => {
                    self.dead.store(true, Ordering::SeqCst);
                    return Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                        dead_session_text(&self.base_url),
                    )]));
                }
            };

            // Extract JSON from SSE data lines
            for line in body.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(data.trim()) {
                        if let Some(result) = parsed.get("result") {
                            if let Ok(call_result) =
                                serde_json::from_value::<CallToolResult>(result.clone())
                            {
                                return Ok(call_result);
                            }
                        }
                    }
                }
            }

            self.dead.store(true, Ordering::SeqCst);
            return Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                dead_session_text(&self.base_url),
            )]));
        }

        // JSON response
        let body: serde_json::Value = match resp.json().await {
            Ok(b) => b,
            Err(_) => {
                self.dead.store(true, Ordering::SeqCst);
                return Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                    dead_session_text(&self.base_url),
                )]));
            }
        };

        if let Some(result) = body.get("result") {
            match serde_json::from_value::<CallToolResult>(result.clone()) {
                Ok(r) => return Ok(r),
                Err(_) => {
                    self.dead.store(true, Ordering::SeqCst);
                }
            }
        }

        Ok(CallToolResult::success(vec![rmcp::model::Content::text(
            dead_session_text(&self.base_url),
        )]))
    }

    /// Check if this proxy is in dead state.
    #[allow(dead_code)] // Available for diagnostics
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    /// Get the base URL of the serve instance.
    #[allow(dead_code)] // Available for diagnostics
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}
