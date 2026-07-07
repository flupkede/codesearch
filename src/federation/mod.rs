//! Federation client — query remote `codesearch serve` peers over HTTP(S) for
//! cross-instance result merging.
//!
//! A group in `repos.json` may list `"@<peer>"` members that reference entries
//! in the `remotes` map. The MCP read-only tools resolve such a group into local
//! and remote targets; the remote targets are queried through this client and the
//! results merged with the local ones.
//!
//! # Graceful degradation
//! Every remote call returns an [`Outcome`] — never panics, never bubbles an
//! `?` into the caller's query path. A peer that times out, returns a non-2xx
//! status, or yields a tool error (`_mcp_is_error`) becomes an
//! [`Outcome::Unreachable`] carrying a short reason. The MCP layer turns those
//! into `warnings` on the response so one bad peer can never fail an otherwise
//! healthy query.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::db_discovery::repos::RemotePeer;
use crate::index::build_serve_client_with_key;

// Per-peer request timeout when none is configured (`timeout_secs = None`).
// Shared with the `remote` CLI command via constants (single source of truth).
use crate::constants::DEFAULT_REMOTE_TIMEOUT_SECS as DEFAULT_TIMEOUT_SECS;

/// A single hit returned by a remote `/search` endpoint.
///
/// Fields mirror the local search-item shapes (semantic *and* literal) but are
/// all optional / defaulted so a slightly older remote that omits a field is
/// tolerated rather than rejecting the whole payload.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RemoteSearchItem {
    /// Remote chunk id (semantic results only; `None` for literal hits).
    #[serde(default)]
    pub chunk_id: Option<u32>,
    /// File path (already alias-prefixed by the remote for multi-repo).
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub start_line: usize,
    #[serde(default)]
    pub end_line: usize,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub score: f32,
    #[serde(default)]
    pub signature: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    /// Literal-mode matched line snippet.
    #[serde(default)]
    pub snippet: Option<String>,
    #[serde(default)]
    pub context_prev: Option<String>,
    #[serde(default)]
    pub context_next: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RemoteSearchResponse {
    #[serde(default)]
    results: Vec<RemoteSearchItem>,
    /// Set by the REST layer when the remote tool returned an MCP error.
    #[serde(default)]
    _mcp_is_error: Option<bool>,
}

/// The outcome of a single remote fan-out call.
#[derive(Debug)]
pub enum Outcome<T> {
    /// The peer answered successfully.
    Ok(T),
    /// The peer was unreachable or errored — degrade gracefully.
    Unreachable(String),
}

/// The outcome of a *management* call (`index … --remote <peer>`):
/// [`list_repos`](FederationClient::list_repos),
/// [`add_repo`](FederationClient::add_repo),
/// [`remove_repo`](FederationClient::remove_repo),
/// [`reindex`](FederationClient::reindex).
///
/// Richer than the query-path [`Outcome`] because management commands are
/// interactive (a human invoked them directly) and must distinguish "peer
/// unreachable" from "peer rejected the request" so the CLI can print a precise
/// message and set the right exit code. A `409 conflict` is not a transport
/// failure — the peer answered; it just said no.
#[derive(Debug)]
pub enum ManagementOutcome<T> {
    /// The peer answered with a 2xx status.
    Ok(T),
    /// The peer answered with a non-2xx status (conflict, not found, warmup
    /// lock, …). Carries the HTTP status code and the peer's `error`/`message`
    /// text so the CLI can surface exactly what the peer reported.
    HttpError {
        /// HTTP status code returned by the peer (e.g. 404, 409, 500).
        status: u16,
        /// Human-readable reason extracted from the peer's JSON `error`/`message`
        /// field, or the raw body when it wasn't JSON.
        reason: String,
    },
    /// The peer did not answer at all (connection refused, timeout, DNS failure,
    /// non-UTF8 / non-JSON body on a success path).
    Unreachable(String),
}

/// `GET /status` payload as served by `codesearch serve`. Only the fields the
/// management commands care about are typed; every field is optional/defaulted
/// so an older or newer remote that adds/omits fields still parses.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RemoteStatus {
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub repos: Vec<RemoteRepoStatus>,
    #[serde(default)]
    pub uptime_secs: Option<u64>,
    #[serde(default)]
    pub active_sessions: Option<u64>,
}

/// A single repo entry inside a [`RemoteStatus`] payload.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RemoteRepoStatus {
    #[serde(default)]
    pub alias: String,
    /// Repo lifecycle state reported by the server
    /// (`open`/`warm`/`readonly`/`closed`/`indexing`/`error`/`no_index`).
    #[serde(default)]
    pub status: String,
    /// `write`/`read`/`-`.
    #[serde(default)]
    pub lock_mode: String,
    #[serde(default)]
    pub changes: u64,
    #[serde(default)]
    pub last_tool_call: Option<String>,
    #[serde(default)]
    pub tool_call_count: Option<u64>,
}

/// `GET /repos/:alias/info` payload — on-disk index stats for one repo on the
/// peer. Only the fields the TUI mount-info overlay renders are typed; every
/// field is optional/defaulted so an older/newer remote still parses.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RemoteRepoInfo {
    #[serde(default)]
    pub chunks: usize,
    #[serde(default)]
    pub files: usize,
    #[serde(default)]
    pub db_size_human: String,
    #[serde(default)]
    pub model: String,
}

/// `POST /repos` success payload (HTTP 202).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RemoteRepoAdded {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub alias: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub message: Option<String>,
}

/// `DELETE /repos/:alias` success payload (HTTP 200).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RemoteRepoRemoved {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub alias: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// `POST /repos/:alias/reindex` success payload (HTTP 202).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RemoteReindexResult {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub alias: String,
    #[serde(default)]
    pub message: Option<String>,
}

/// HTTP client for talking to remote `codesearch serve` peers.
///
/// Holds a single `reqwest::Client` (rustls, no default auth header); the
/// per-peer API key is attached to each request via `bearer_auth`. Built on top
/// of [`build_serve_client_with_key`] so transport configuration (TLS backend,
/// builder error handling) stays in one place.
pub struct FederationClient {
    client: reqwest::Client,
}

impl Clone for FederationClient {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
        }
    }
}

impl FederationClient {
    /// Build a federation client. Returns an error only if the underlying HTTP
    /// client cannot be constructed (e.g. TLS backend init failure).
    pub fn new() -> Result<Self, String> {
        // Blanket timeout as a safety upper bound; each request also gets the
        // peer's own (usually shorter) timeout via `RequestBuilder::timeout`.
        let client = build_serve_client_with_key(
            std::time::Duration::from_secs(180),
            None, // no default auth header — keys are per-peer
        )?;
        Ok(Self { client })
    }

    fn peer_url(peer: &RemotePeer, suffix: &str) -> String {
        format!("{}{}", peer.url.trim_end_matches('/'), suffix)
    }

    fn peer_timeout(peer: &RemotePeer) -> std::time::Duration {
        std::time::Duration::from_secs(peer.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS))
    }

    /// Query a remote peer's `/search` endpoint.
    ///
    /// `body` is the local search request, serialised as JSON; `group` on the
    /// body is forced to the peer's configured group (or `"all"` when unset) and
    /// `project` is stripped, because projects are local to each instance.
    /// Query a remote peer's `/search` endpoint scoped to a SINGLE remote
    /// project (project-level federation / mounted remote project).
    ///
    /// Forces `project=<remote_alias>` and strips `group`: the peer resolves the
    /// project in its own namespace and returns only that project's results.
    /// `remote_alias` is the project's bare name on the peer (the `<alias>` half
    /// of the local `<peer>/<alias>` mount). This is the ONLY search path —
    /// group federation fans out to each mounted project via this method, so a
    /// query only ever touches the individual indexes the user opted into.
    pub async fn search_project(
        &self,
        peer: &RemotePeer,
        mut body: serde_json::Value,
        remote_alias: &str,
    ) -> Outcome<Vec<RemoteSearchItem>> {
        if let Some(obj) = body.as_object_mut() {
            obj.insert(
                "project".into(),
                serde_json::Value::String(remote_alias.to_string()),
            );
            obj.remove("group");
        }
        self.post_search(peer, body).await
    }

    /// Shared POST + parse for `/search` (group- and project-scoped variants
    /// prepare the body differently, then funnel through here).
    async fn post_search(
        &self,
        peer: &RemotePeer,
        body: serde_json::Value,
    ) -> Outcome<Vec<RemoteSearchItem>> {
        let url = Self::peer_url(peer, crate::constants::SEARCH_PATH);
        let req = self
            .client
            .post(&url)
            .timeout(Self::peer_timeout(peer))
            .json(&body);
        let req = attach_bearer(req, &peer.api_key);

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                match resp.json::<RemoteSearchResponse>().await {
                    Ok(parsed) if status.is_success() && !parsed._mcp_is_error.unwrap_or(false) => {
                        Outcome::Ok(parsed.results)
                    }
                    Ok(parsed) => {
                        // Tool-level error on the remote (e.g. scope_required).
                        let n = parsed.results.len();
                        Outcome::Unreachable(format!(
                            "remote /search returned a tool error (http={status}, items={n})"
                        ))
                    }
                    Err(e) => Outcome::Unreachable(format!(
                        "remote /search returned non-JSON body (http={status}): {e}"
                    )),
                }
            }
            Err(e) => Outcome::Unreachable(format!("remote /search unreachable: {e}")),
        }
    }

    /// Fetch a single chunk from a remote peer's `/chunk/:id` endpoint.
    ///
    /// Scoping mirrors [`Self::search_project`]:
    /// - When `remote_alias` is `Some`, the lookup is scoped to that single
    ///   project via `project=<alias>` and `group` is omitted. This is required
    ///   because the peer is multi-repo and chunk_ids collide across its
    ///   indexes — a group/`all`-scoped lookup returns `ambiguous_chunk_id`.
    /// - When `remote_alias` is `None` (legacy non-namespaced `chunk_ref`), the
    ///   lookup falls back to the peer's configured group (or `all`).
    ///
    /// Returns the raw `GetChunkResponse` JSON produced by the remote tool.
    pub async fn get_chunk(
        &self,
        peer: &RemotePeer,
        remote_alias: Option<&str>,
        chunk_id: u32,
        context_lines: Option<usize>,
    ) -> Outcome<serde_json::Value> {
        let mut url = Self::peer_url(
            peer,
            &crate::constants::CHUNK_PATH.replace(":id", &chunk_id.to_string()),
        );
        // Scope the lookup: prefer a single-project scope (`project=<alias>`)
        // so the multi-repo peer can disambiguate the chunk_id; fall back to
        // the peer's group only for legacy non-namespaced refs.
        let mut qs: Vec<(String, String)> = match remote_alias {
            Some(alias) => vec![("project".to_string(), alias.to_string())],
            None => {
                let group = peer
                    .group
                    .clone()
                    .unwrap_or_else(|| crate::constants::ALL_GROUP_NAME.to_string());
                vec![("group".to_string(), group)]
            }
        };
        if let Some(cl) = context_lines {
            qs.push(("context_lines".to_string(), cl.to_string()));
        }
        let query = qs
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding(k), urlencoding(v)))
            .collect::<Vec<_>>()
            .join("&");
        url.push('?');
        url.push_str(&query);
        let req = self.client.get(&url).timeout(Self::peer_timeout(peer));
        let req = attach_bearer(req, &peer.api_key);
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                match resp.json::<serde_json::Value>().await {
                    Ok(v) if status.is_success() && !is_mcp_error(&v) => Outcome::Ok(v),
                    Ok(v) => Outcome::Unreachable(format!(
                        "remote /chunk returned a tool error (http={status}): {}",
                        short_reason(&v)
                    )),
                    Err(e) => Outcome::Unreachable(format!(
                        "remote /chunk returned non-JSON body (http={status}): {e}"
                    )),
                }
            }
            Err(e) => Outcome::Unreachable(format!("remote /chunk unreachable: {e}")),
        }
    }

    /// Shared request/response handling for the management endpoints
    /// (`/status`, `/repos`, `/repos/:alias`, `/repos/:alias/reindex`).
    ///
    /// Distinguishes three failure modes (see [`ManagementOutcome`]):
    /// transport failure → `Unreachable`; non-2xx → `HttpError` with the peer's
    /// own `error`/`message` text; success → deserialised into `T`. Reading the
    /// body as text first lets us surface the raw payload on a parse error
    /// instead of a generic "non-JSON" message.
    async fn send_management<T: DeserializeOwned>(
        &self,
        peer: &RemotePeer,
        method: reqwest::Method,
        suffix: &str,
        body: Option<&serde_json::Value>,
        query: Option<&str>,
    ) -> ManagementOutcome<T> {
        let mut url = Self::peer_url(peer, suffix);
        if let Some(q) = query {
            url.push('?');
            url.push_str(q);
        }
        let mut req = self
            .client
            .request(method, &url)
            .timeout(Self::peer_timeout(peer));
        if let Some(b) = body {
            req = req.json(b);
        }
        let req = attach_bearer(req, &peer.api_key);

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return ManagementOutcome::Unreachable(format!("{url} unreachable: {e}")),
        };
        let status = resp.status();
        let text = match resp.text().await {
            Ok(t) => t,
            Err(e) => {
                return ManagementOutcome::Unreachable(format!(
                    "{url} returned an unreadable body (http={status}): {e}"
                ))
            }
        };
        if status.is_success() {
            match serde_json::from_str::<T>(&text) {
                Ok(v) => ManagementOutcome::Ok(v),
                Err(e) => ManagementOutcome::Unreachable(format!(
                    "{url} returned a body that did not parse (http={status}): {e}"
                )),
            }
        } else {
            // Surface the peer's own error/message field when present; fall back
            // to the raw body so 4xx/5xx diagnostics are never lost.
            let reason = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .map(|v| short_reason(&v))
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| {
                    if text.trim().is_empty() {
                        "<no detail>".to_string()
                    } else {
                        text.trim().to_string()
                    }
                });
            ManagementOutcome::HttpError {
                status: status.as_u16(),
                reason,
            }
        }
    }

    /// `GET /status` — list every repo known to the peer plus its runtime state.
    pub async fn list_repos(&self, peer: &RemotePeer) -> ManagementOutcome<RemoteStatus> {
        self.send_management(
            peer,
            reqwest::Method::GET,
            crate::constants::STATUS_PATH,
            None,
            None,
        )
        .await
    }

    /// `POST /repos { path }` — register a repo on the peer. `path` is a path
    /// on the **peer's** filesystem; the flag does not stat anything locally.
    /// Returns 202 on accept, 409 if already registered.
    pub async fn add_repo(
        &self,
        peer: &RemotePeer,
        path: &str,
    ) -> ManagementOutcome<RemoteRepoAdded> {
        let body = serde_json::json!({ "path": path });
        self.send_management(
            peer,
            reqwest::Method::POST,
            crate::constants::REPOS_PATH,
            Some(&body),
            None,
        )
        .await
    }

    /// `DELETE /repos/:alias` — unregister a repo on the peer and delete its DB.
    /// `alias` is the peer's repo alias (NOT a local path).
    pub async fn remove_repo(
        &self,
        peer: &RemotePeer,
        alias: &str,
    ) -> ManagementOutcome<RemoteRepoRemoved> {
        // Build the per-repo path from the neutral REPOS_PATH collection route
        // (not REPO_REINDEX_PATH_PREFIX, whose name implies reindex-only use).
        let suffix = format!("{}/{}", crate::constants::REPOS_PATH, urlencoding(alias));
        self.send_management(peer, reqwest::Method::DELETE, &suffix, None, None)
            .await
    }

    /// `GET /repos/:alias/info` — fetch on-disk index stats (chunks/files/db
    /// size/model) for one repo on the peer. `alias` is the peer's repo alias.
    pub async fn repo_info(
        &self,
        peer: &RemotePeer,
        alias: &str,
    ) -> ManagementOutcome<RemoteRepoInfo> {
        let suffix = format!(
            "{}/{}{}",
            crate::constants::REPOS_PATH,
            urlencoding(alias),
            crate::constants::REPO_INFO_PATH_SUFFIX,
        );
        self.send_management(peer, reqwest::Method::GET, &suffix, None, None)
            .await
    }

    /// `POST /repos/:alias/reindex[?force=true]` — trigger a background
    /// incremental (or forced full) reindex of a repo on the peer.
    pub async fn reindex(
        &self,
        peer: &RemotePeer,
        alias: &str,
        force: bool,
    ) -> ManagementOutcome<RemoteReindexResult> {
        let suffix = format!(
            "{}/{}{}",
            crate::constants::REPOS_PATH,
            urlencoding(alias),
            crate::constants::REPO_REINDEX_PATH_SUFFIX,
        );
        let query = if force { Some("force=true") } else { None };
        self.send_management(peer, reqwest::Method::POST, &suffix, None, query)
            .await
    }
}

fn attach_bearer(req: reqwest::RequestBuilder, api_key: &str) -> reqwest::RequestBuilder {
    if api_key.trim().is_empty() {
        req
    } else {
        req.bearer_auth(api_key)
    }
}

fn is_mcp_error(v: &serde_json::Value) -> bool {
    v.get("_mcp_is_error")
        .and_then(|b| b.as_bool())
        .unwrap_or(false)
}

fn short_reason(v: &serde_json::Value) -> String {
    v.get("error")
        .and_then(|e| e.as_str())
        .or_else(|| v.get("message").and_then(|m| m.as_str()))
        .unwrap_or("<no detail>")
        .to_string()
}

/// Minimal percent-encoding for query values (avoids pulling in a new crate
/// just for `:`/`/`/space in URLs). Encodes everything except unreserved chars.
fn urlencoding(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db_discovery::repos::RemotePeer;

    fn peer(url: String) -> RemotePeer {
        RemotePeer {
            url,
            api_key: String::new(),
            group: None,
            timeout_secs: Some(5),
        }
    }

    /// Bind an ephemeral port, serve `app`, and return the address only once the
    /// listener actually accepts a TCP connection. This bounded readiness poll
    /// replaces a fixed `sleep(50ms)`, which occasionally lost the startup race
    /// when the suite ran many tests (and other `cargo` processes) in parallel.
    async fn spawn_test_server(app: axum::Router) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        for _ in 0..200 {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                return addr;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("test server at {addr} never became ready");
    }

    #[test]
    fn urlencoding_encodes_reserved_and_passes_unreserved() {
        assert_eq!(urlencoding("a-b_c.d~"), "a-b_c.d~");
        // Space, colon, slash, non-ascii → percent-encoded.
        assert_eq!(urlencoding("a b"), "a%20b");
        assert_eq!(urlencoding("a:b/c"), "a%3Ab%2Fc");
        assert_eq!(urlencoding("é"), "%C3%A9");
    }

    #[tokio::test]
    async fn search_unreachable_returns_degraded_outcome() {
        // Bind a port then drop it so the address refuses connections.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let client = FederationClient::new().unwrap();
        let outcome = client
            .search_project(
                &peer(format!("http://{addr}")),
                serde_json::json!({"query": "x"}),
                "kb",
            )
            .await;
        match outcome {
            Outcome::Unreachable(_) => {}
            other => panic!("expected Unreachable, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn search_returns_results_from_a_live_peer() {
        let app = axum::Router::new().route(
            crate::constants::SEARCH_PATH,
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({
                    "results": [{
                        "chunk_id": 7,
                        "path": "kb/doc.md",
                        "start_line": 1,
                        "end_line": 4,
                        "kind": "Section",
                        "score": 0.5
                    }]
                }))
            }),
        );
        let addr = spawn_test_server(app).await;

        let client = FederationClient::new().unwrap();
        let outcome = client
            .search_project(
                &peer(format!("http://{addr}")),
                serde_json::json!({"query": "x"}),
                "kb",
            )
            .await;
        match outcome {
            Outcome::Ok(items) => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].chunk_id, Some(7));
                assert_eq!(items[0].path, "kb/doc.md");
            }
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn search_project_forces_project_and_strips_group() {
        use std::sync::{Arc, Mutex};

        // Capture the exact body the peer receives so we can assert on scoping.
        let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let cap = captured.clone();
        let app = axum::Router::new().route(
            crate::constants::SEARCH_PATH,
            axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let cap = cap.clone();
                async move {
                    *cap.lock().unwrap() = Some(body);
                    axum::Json(serde_json::json!({ "results": [] }))
                }
            }),
        );
        let addr = spawn_test_server(app).await;

        // Peer carries a group; search_project MUST override it with the project.
        let mut p = peer(format!("http://{addr}"));
        p.group = Some("some-remote-group".into());

        let outcome = client_new()
            .search_project(
                &p,
                serde_json::json!({ "query": "x", "group": "leftover", "mode": "semantic" }),
                "vendor-a",
            )
            .await;
        assert!(matches!(outcome, Outcome::Ok(_)));

        let body = captured
            .lock()
            .unwrap()
            .clone()
            .expect("peer received a body");
        assert_eq!(
            body.get("project").and_then(|v| v.as_str()),
            Some("vendor-a"),
            "project must be forced to the remote alias"
        );
        assert!(
            body.get("group").is_none(),
            "group must be stripped for single-project routing, got: {body}"
        );
    }

    fn client_new() -> FederationClient {
        FederationClient::new().unwrap()
    }

    #[tokio::test]
    async fn get_chunk_fetches_from_a_live_peer() {
        // The handler echoes back the scoping query params it received so the
        // test can assert the client forwards `project=<alias>` (and NOT a
        // `group`) for a namespaced lookup — the fix for `ambiguous_chunk_id`
        // on a multi-repo peer.
        let app = axum::Router::new().route(
            "/chunk/:id",
            axum::routing::get(
                |axum::extract::Query(params): axum::extract::Query<
                    std::collections::HashMap<String, String>,
                >| async move {
                    axum::Json(serde_json::json!({
                        "chunk_id": 7,
                        "path": "kb/doc.md",
                        "content": "the chunk body",
                        "received_project": params.get("project"),
                        "received_group": params.get("group"),
                    }))
                },
            ),
        );
        let addr = spawn_test_server(app).await;

        let client = FederationClient::new().unwrap();
        let outcome = client
            .get_chunk(&peer(format!("http://{addr}")), Some("inriver"), 7, None)
            .await;
        match outcome {
            Outcome::Ok(value) => {
                assert_eq!(value.get("chunk_id").and_then(|v| v.as_u64()), Some(7));
                assert_eq!(
                    value.get("content").and_then(|v| v.as_str()),
                    Some("the chunk body")
                );
                // Namespaced lookup: project scope forwarded, group omitted.
                assert_eq!(
                    value.get("received_project").and_then(|v| v.as_str()),
                    Some("inriver"),
                    "project=<alias> must be forwarded to disambiguate the multi-repo peer"
                );
                assert!(
                    value
                        .get("received_group")
                        .map(|v| v.is_null())
                        .unwrap_or(true),
                    "group must be omitted when a project scope is used, got: {value}"
                );
            }
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn get_chunk_legacy_no_alias_falls_back_to_group() {
        // A legacy (non-namespaced) chunk_ref yields `remote_alias == None`; the
        // lookup must then fall back to the peer's group scope and NOT send a
        // `project` param — preserving pre-fix behaviour for old refs.
        let app = axum::Router::new().route(
            "/chunk/:id",
            axum::routing::get(
                |axum::extract::Query(params): axum::extract::Query<
                    std::collections::HashMap<String, String>,
                >| async move {
                    axum::Json(serde_json::json!({
                        "chunk_id": 7,
                        "received_project": params.get("project"),
                        "received_group": params.get("group"),
                    }))
                },
            ),
        );
        let addr = spawn_test_server(app).await;

        let client = FederationClient::new().unwrap();
        let outcome = client
            .get_chunk(&peer(format!("http://{addr}")), None, 7, None)
            .await;
        match outcome {
            Outcome::Ok(value) => {
                assert!(
                    value
                        .get("received_project")
                        .map(|v| v.is_null())
                        .unwrap_or(true),
                    "legacy lookup must not send a project param, got: {value}"
                );
                // `peer()` configures group "all" (or the peer's group), which must be forwarded.
                assert!(
                    value
                        .get("received_group")
                        .and_then(|v| v.as_str())
                        .is_some(),
                    "legacy lookup must forward a group scope, got: {value}"
                );
            }
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    // --- management methods (list/add/remove/reindex) ---

    #[tokio::test]
    async fn list_repos_returns_status_from_a_live_peer() {
        let app = axum::Router::new().route(
            crate::constants::STATUS_PATH,
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "version": "1.0.0",
                    "repos": [
                        {"alias": "docs", "status": "open", "lock_mode": "read", "changes": 3},
                        {"alias": "inriver", "status": "warm", "lock_mode": "-", "changes": 0}
                    ],
                    "active_sessions": 1,
                    "uptime_secs": 600
                }))
            }),
        );
        let addr = spawn_test_server(app).await;

        let client = FederationClient::new().unwrap();
        let outcome = client.list_repos(&peer(format!("http://{addr}"))).await;
        match outcome {
            ManagementOutcome::Ok(status) => {
                assert_eq!(status.version.as_deref(), Some("1.0.0"));
                assert_eq!(status.repos.len(), 2);
                assert_eq!(status.repos[0].alias, "docs");
                assert_eq!(status.repos[0].status, "open");
                assert_eq!(status.repos[0].changes, 3);
                assert_eq!(status.repos[1].alias, "inriver");
                assert_eq!(status.repos[1].lock_mode, "-");
                assert_eq!(status.active_sessions, Some(1));
            }
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn add_repo_forwards_path_and_parses_accepted() {
        // Echo the received `path` back so we verify it was forwarded.
        let app = axum::Router::new().route(
            crate::constants::REPOS_PATH,
            axum::routing::post(
                |axum::Json(body): axum::Json<serde_json::Value>| async move {
                    let path = body
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    axum::Json(serde_json::json!({
                        "status": "accepted",
                        "alias": "docs",
                        "path": path,
                        "message": "Reindex started in background"
                    }))
                },
            ),
        );
        let addr = spawn_test_server(app).await;

        let client = FederationClient::new().unwrap();
        let outcome = client
            .add_repo(&peer(format!("http://{addr}")), "/app/docs")
            .await;
        match outcome {
            ManagementOutcome::Ok(added) => {
                assert_eq!(added.status, "accepted");
                assert_eq!(added.alias, "docs");
                assert_eq!(added.path, "/app/docs");
                assert_eq!(
                    added.message.as_deref(),
                    Some("Reindex started in background")
                );
            }
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn remove_repo_targets_alias_in_url() {
        // Echo the captured alias back to prove it landed in the DELETE path.
        let app = axum::Router::new().route(
            "/repos/:alias",
            axum::routing::delete(
                |axum::extract::Path(alias): axum::extract::Path<String>| async move {
                    axum::Json(serde_json::json!({
                        "status": "removed",
                        "alias": alias,
                        "message": "unregistered"
                    }))
                },
            ),
        );
        let addr = spawn_test_server(app).await;

        let client = FederationClient::new().unwrap();
        let outcome = client
            .remove_repo(&peer(format!("http://{addr}")), "inriver")
            .await;
        match outcome {
            ManagementOutcome::Ok(removed) => {
                assert_eq!(removed.status, "removed");
                assert_eq!(removed.alias, "inriver");
                assert_eq!(removed.message.as_deref(), Some("unregistered"));
            }
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn reindex_posts_to_alias_reindex_path() {
        // Capture the alias from the path to prove the reindex URL was built.
        let app = axum::Router::new().route(
            "/repos/:alias/reindex",
            axum::routing::post(
                |axum::extract::Path(alias): axum::extract::Path<String>| async move {
                    axum::Json(serde_json::json!({
                        "status": "accepted",
                        "alias": alias,
                        "message": "Reindex started in background"
                    }))
                },
            ),
        );
        let addr = spawn_test_server(app).await;

        let client = FederationClient::new().unwrap();
        let outcome = client
            .reindex(&peer(format!("http://{addr}")), "inriver", false)
            .await;
        match outcome {
            ManagementOutcome::Ok(res) => {
                assert_eq!(res.status, "accepted");
                assert_eq!(res.alias, "inriver");
                assert_eq!(
                    res.message.as_deref(),
                    Some("Reindex started in background")
                );
            }
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn reindex_with_force_appends_force_query() {
        // Capture the query string to prove ?force=true was forwarded.
        let app = axum::Router::new().route(
            "/repos/:alias/reindex",
            axum::routing::post(
                |axum::extract::Query(params): axum::extract::Query<
                    std::collections::HashMap<String, String>,
                >| async move {
                    let force = params.get("force").map(String::as_str).unwrap_or("");
                    axum::Json(serde_json::json!({
                        "status": "accepted",
                        "alias": "inriver",
                        "message": format!("force={force}")
                    }))
                },
            ),
        );
        let addr = spawn_test_server(app).await;

        let client = FederationClient::new().unwrap();
        // force=true must arrive at the peer.
        let outcome = client
            .reindex(&peer(format!("http://{addr}")), "inriver", true)
            .await;
        match outcome {
            ManagementOutcome::Ok(res) => {
                assert_eq!(res.message.as_deref(), Some("force=true"));
            }
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn remove_repo_urlencodes_alias_in_path() {
        // An alias with a space must be percent-encoded on the wire and decoded
        // back by axum — proves the encoding round-trips through the HTTP layer.
        let app = axum::Router::new().route(
            "/repos/:alias",
            axum::routing::delete(
                |axum::extract::Path(alias): axum::extract::Path<String>| async move {
                    axum::Json(serde_json::json!({
                        "status": "removed",
                        "alias": alias,
                        "message": "unregistered"
                    }))
                },
            ),
        );
        let addr = spawn_test_server(app).await;

        let client = FederationClient::new().unwrap();
        let outcome = client
            .remove_repo(&peer(format!("http://{addr}")), "my repo")
            .await;
        match outcome {
            ManagementOutcome::Ok(removed) => {
                // axum decodes %20 → space, so the echoed alias must match input.
                assert_eq!(removed.alias, "my repo");
                assert_eq!(removed.status, "removed");
            }
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn management_http_error_surfaces_peer_reason() {
        // Peer rejects with 409 conflict — must become HttpError, not Unreachable.
        let app = axum::Router::new().route(
            crate::constants::REPOS_PATH,
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::CONFLICT,
                    axum::Json(serde_json::json!({
                        "error": "already registered",
                        "status": "conflict",
                        "alias": "docs"
                    })),
                )
            }),
        );
        let addr = spawn_test_server(app).await;

        let client = FederationClient::new().unwrap();
        let outcome = client
            .add_repo(&peer(format!("http://{addr}")), "/app/docs")
            .await;
        match outcome {
            ManagementOutcome::HttpError { status, reason } => {
                assert_eq!(status, 409);
                assert_eq!(reason, "already registered");
            }
            other => panic!("expected HttpError, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn management_unreachable_when_peer_is_down() {
        // Bind then drop so the address refuses connections.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let client = FederationClient::new().unwrap();
        let outcome = client
            .remove_repo(&peer(format!("http://{addr}")), "inriver")
            .await;
        match outcome {
            ManagementOutcome::Unreachable(_) => {}
            other => panic!("expected Unreachable, got {:?}", other),
        }
    }
}
