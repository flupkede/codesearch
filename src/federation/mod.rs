//! Federation client — query remote `codesearch serve` peers over HTTP(S) for
//! cross-instance result merging (see `docs/federation-feature.md`).
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

use serde::Deserialize;

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
#[derive(Debug, Clone, Default, Deserialize)]
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
    pub async fn search(
        &self,
        peer: &RemotePeer,
        mut body: serde_json::Value,
    ) -> Outcome<Vec<RemoteSearchItem>> {
        // Force the scope onto the remote's own group/namespace.
        if let Some(obj) = body.as_object_mut() {
            let g = peer
                .group
                .clone()
                .unwrap_or_else(|| crate::constants::ALL_GROUP_NAME.to_string());
            obj.insert("group".into(), serde_json::Value::String(g));
            obj.remove("project");
        }
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
    /// `group` is forced to the peer's configured group so the remote searches
    /// the right scope. Returns the raw `GetChunkResponse` JSON produced by the
    /// remote tool.
    pub async fn get_chunk(
        &self,
        peer: &RemotePeer,
        chunk_id: u32,
        context_lines: Option<usize>,
    ) -> Outcome<serde_json::Value> {
        let group = peer
            .group
            .clone()
            .unwrap_or_else(|| crate::constants::ALL_GROUP_NAME.to_string());
        let mut url = Self::peer_url(
            peer,
            &crate::constants::CHUNK_PATH.replace(":id", &chunk_id.to_string()),
        );
        // Build a query string: group always, context_lines when present.
        let mut qs = vec![("group".to_string(), group)];
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
            .search(
                &peer(format!("http://{addr}")),
                serde_json::json!({"query": "x"}),
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let client = FederationClient::new().unwrap();
        let outcome = client
            .search(
                &peer(format!("http://{addr}")),
                serde_json::json!({"query": "x"}),
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
    async fn get_chunk_fetches_from_a_live_peer() {
        let app = axum::Router::new().route(
            // axum route for /chunk/:id
            "/chunk/:id",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "chunk_id": 7,
                    "path": "kb/doc.md",
                    "content": "the chunk body"
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let client = FederationClient::new().unwrap();
        let outcome = client
            .get_chunk(&peer(format!("http://{addr}")), 7, None)
            .await;
        match outcome {
            Outcome::Ok(value) => {
                assert_eq!(value.get("chunk_id").and_then(|v| v.as_u64()), Some(7));
                assert_eq!(
                    value.get("content").and_then(|v| v.as_str()),
                    Some("the chunk body")
                );
            }
            other => panic!("expected Ok, got {:?}", other),
        }
    }
}
