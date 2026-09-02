use rmcp::{
    model::{
        CallToolRequestParams, CallToolResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
    ErrorData as McpError, RoleClient, RoleServer, ServerHandler,
};
use std::sync::{Arc, Mutex};

// ═══════════════════════════════════════════════════════════════════
// MCP Proxy Service  (--mode client / --mode auto with serve detected)
// ═══════════════════════════════════════════════════════════════════

/// Transparent stdio↔HTTP proxy with automatic reconnect.
///
/// When `codesearch mcp --mode client` is started by Claude Desktop:
/// - Claude Desktop sends MCP requests over stdio
/// - `McpProxyService` forwards every request to the running `codesearch serve` hub via HTTP
/// - Responses flow back unchanged
///
/// This is the correct architecture for Claude Desktop: it has no repo context of its own
/// and therefore cannot use `--mode local`. With `--mode client` it always connects to
/// the serve hub, gaining access to all registered repos.
///
/// Only tool operations (`list_tools`, `call_tool`) are forwarded. Prompts, resources,
/// and completion are not proxied — the serve hub does not expose them.
///
/// ## Reconnect
///
/// The peer is wrapped in `Arc<RwLock<Option<Peer>>>` so it can be hot-swapped when the
/// serve connection drops and reconnects. During reconnection, tool calls return a
/// descriptive "reconnecting" error so Claude Desktop can retry.
///
/// ## Idle disconnect / connect on demand
///
/// The peer is also `None` while the proxy is *deliberately* disconnected: after
/// `CODESEARCH_MCP_PROXY_IDLE_DISCONNECT_SECS` (default
/// `DEFAULT_MCP_PROXY_IDLE_DISCONNECT_SECS`, `0` disables) without a successful
/// forwarded request, the idle-checker in `run_mcp_client` closes the HTTP MCP
/// session so a scale-to-zero remote can suspend its replica. Every successful
/// `list_tools` / `call_tool` stamps `last_activity`, which resets that window.
///
/// Because a closed session is indistinguishable from a dead one at the peer
/// slot, `call_tool` / `list_tools` signal `connect_request_tx` on their first
/// attempt whenever the slot is `None`, asking the main loop to connect *now*
/// instead of waiting for the failure-path reconnect cadence. The existing
/// bounded retry-with-backoff remains the fallback if that connect does not land
/// within the retry budget.
pub(crate) struct McpProxyService {
    /// Shared peer handle — hot-swapped on reconnect.
    /// `None` means we're reconnecting to serve; tool calls return a retry-able error.
    pub(crate) peer: std::sync::Arc<tokio::sync::RwLock<Option<rmcp::service::Peer<RoleClient>>>>,
    /// Signal to the main loop in `run_mcp_client` that the current peer is dead
    /// and a fresh `connect_to_serve` should be attempted. Sent from `call_tool` /
    /// `list_tools` when rmcp returns a transport-level error so we can recover
    /// from server restarts and TCP keep-alive failures without bubbling the error
    /// up to Claude Desktop.
    pub(crate) disconnect_tx: tokio::sync::mpsc::Sender<()>,
    /// Ask the main loop to run `connect_to_serve` immediately (capacity-1
    /// channel — duplicate requests coalesce, "connect now" is idempotent).
    /// Sent when a request arrives while the peer slot is empty.
    pub(crate) connect_request_tx: tokio::sync::mpsc::Sender<()>,
    /// When the last request was successfully forwarded to serve. Shared with the
    /// idle-checker in `run_mcp_client`, which closes the connection once this is
    /// older than the configured idle-disconnect window.
    pub(crate) last_activity: Arc<Mutex<std::time::Instant>>,
    /// Number of requests currently being forwarded. `last_activity` only advances
    /// on completion, so without this a request that runs longer than the idle
    /// window (a big search, a cold symbol rebuild) would have its own transport
    /// closed underneath it. The idle-checker never disconnects while this is > 0.
    pub(crate) in_flight: Arc<std::sync::atomic::AtomicUsize>,
    /// Notified by the main loop's `connect_request_rx` arm whenever an on-demand
    /// `connect_to_serve` attempt returns `Err` — i.e. serve refused the
    /// connection outright, as opposed to still being slow to accept one. Lets
    /// `await_peer` stop waiting immediately on a definitive failure instead of
    /// polling out the rest of `PROXY_CONNECT_WAIT_MS` (previously ~20s per call
    /// even when serve was known to be down within the first few milliseconds).
    /// A slow-but-eventually-successful wake never touches this: it resolves by
    /// the peer slot filling in, which `await_peer`'s own poll already catches.
    pub(crate) connect_failed: Arc<tokio::sync::Notify>,
}

/// Keeps `McpProxyService::in_flight` incremented for its lifetime. A guard rather
/// than paired add/sub calls because the forwarding loop has several early returns.
struct InFlightGuard(Arc<std::sync::atomic::AtomicUsize>);

impl InFlightGuard {
    fn new(counter: &Arc<std::sync::atomic::AtomicUsize>) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(counter.clone())
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl McpProxyService {
    #[allow(dead_code)]
    fn new(peer: rmcp::service::Peer<RoleClient>) -> Self {
        // Direct constructor used by tests / single-shot scenarios.
        // No reconnect plumbing — the dummy channels are never read.
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let (connect_tx, _connect_rx) = tokio::sync::mpsc::channel(1);
        Self {
            peer: std::sync::Arc::new(tokio::sync::RwLock::new(Some(peer))),
            disconnect_tx: tx,
            connect_request_tx: connect_tx,
            last_activity: Arc::new(Mutex::new(std::time::Instant::now())),
            in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            connect_failed: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Stamp "real traffic just flowed", resetting the idle-disconnect window.
    fn mark_activity(&self) {
        mark_proxy_activity(&self.last_activity);
    }

    /// Best-effort nudge to the main loop: connect to serve now. A full channel
    /// already means "a connect is pending", a closed one means the loop is gone;
    /// both are fine to ignore — the caller's own retry/backoff covers it.
    fn request_connect(&self) {
        let _ = self.connect_request_tx.try_send(());
    }

    /// Wait — bounded by `PROXY_CONNECT_WAIT_MS` — for the peer slot to be filled
    /// after `request_connect`. Returns true as soon as a peer is available.
    ///
    /// Without this, a request arriving after an idle-close would burn its whole
    /// retry budget (~1s) while the on-demand connect is still waking a
    /// scaled-to-zero remote, and fail with "reconnecting" every single time.
    async fn await_peer(&self) -> bool {
        self.await_peer_bounded(PROXY_CONNECT_WAIT_MS).await
    }

    /// Core of `await_peer`, parameterized on the wait budget so it is unit
    /// testable without actually waiting out `PROXY_CONNECT_WAIT_MS` (~20s).
    /// Uses the production refusal-grace window; see
    /// `await_peer_bounded_with_grace` for what that means and why it is its
    /// own parameter.
    async fn await_peer_bounded(&self, wait_ms: u64) -> bool {
        self.await_peer_bounded_with_grace(wait_ms, CONNECT_REFUSAL_GRACE)
            .await
    }

    /// Core of `await_peer_bounded`, additionally parameterized on the
    /// refusal-grace window so *that* is unit testable without waiting out
    /// `reconnect::INTERVAL_SECS` (~3s) for real.
    ///
    /// Polls the peer slot on `PROXY_RETRY_BACKOFF_MS` cadence, but also races
    /// each poll against `connect_failed` so a definitive on-demand connect
    /// failure (serve refused the connection, not merely slow to accept one)
    /// clamps the remaining wait down to `refusal_grace` instead of polling
    /// out the rest of `wait_ms`. A slow-but-still-in-progress wake never
    /// fires `connect_failed` — it is only notified from an `Err` return of
    /// `connect_to_serve` — so this does not shorten the legitimate
    /// scale-to-zero wake path, only the case where serve is already known to
    /// have refused this attempt.
    ///
    /// The clamp is deliberately *not* an immediate return: a refusal only
    /// means this one on-demand attempt was refused, not that serve won't
    /// recover — `run_mcp_client`'s own disconnect/reconnect cycle
    /// (`reconnect::INTERVAL_SECS` later) can still land within the original
    /// budget, e.g. when serve is mid-restart rather than genuinely down.
    /// Returning immediately turned that case — previously transparent to the
    /// caller, since the pre-fix full-budget poll caught the reconnect — into
    /// a visible "reconnecting" error on the very first request after a
    /// restart. Clamping to `refusal_grace` keeps most of the original fix's
    /// win (a hard-down serve is still bounded well under the full `wait_ms`)
    /// while still giving that recovery cycle room to land.
    async fn await_peer_bounded_with_grace(
        &self,
        wait_ms: u64,
        refusal_grace: std::time::Duration,
    ) -> bool {
        let mut deadline = std::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
        loop {
            // Register for the next failure notification BEFORE checking the
            // peer slot, so a failure landing between this check and the
            // `select!` below cannot be missed (the standard tokio::sync::Notify
            // idiom: create the `Notified` future first, await it second).
            let failed = self.connect_failed.notified();
            if self.peer.read().await.is_some() {
                return true;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            let backoff = std::time::Duration::from_millis(PROXY_RETRY_BACKOFF_MS)
                .min(deadline.saturating_duration_since(now));
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = failed => {
                    // A concurrent successful connect could still have landed in
                    // the instant before this notification; one last check keeps
                    // that case correct instead of reporting a false failure.
                    if self.peer.read().await.is_some() {
                        return true;
                    }
                    deadline = deadline.min(now + refusal_grace);
                }
            }
        }
    }

    /// On an empty peer slot (a deliberate idle-close or a real outage), ask the
    /// main loop to connect *now* instead of waiting for the failure-path
    /// reconnect cadence to notice, then give that connect a bounded window to
    /// land. Returns true if a peer became available and the caller should
    /// retry its forwarded call immediately.
    ///
    /// Only meaningful on the caller's first attempt (`attempt == 0`) — a
    /// second empty slot means the on-demand connect already ran and fell
    /// through to the ordinary retry/backoff path. Pulled out of `list_tools`/
    /// `call_tool` because the two copies had already started to drift (see
    /// review remarks on the commit that added this).
    async fn try_on_demand_connect(&self) -> bool {
        self.request_connect();
        self.await_peer().await
    }

    /// Force a reconnect: clear the shared peer and signal the main loop in
    /// `run_mcp_client` to call `connect_to_serve` again. Brief sleep gives
    /// the main loop time to actually reconnect before the caller retries.
    async fn force_reconnect(&self) {
        *self.peer.write().await = None;
        let _ = self.disconnect_tx.send(()).await;
        tokio::time::sleep(std::time::Duration::from_millis(PROXY_RETRY_BACKOFF_MS)).await;
    }
}

/// Maximum number of attempts when forwarding a request to serve.
/// Each retry includes a forced reconnect, so this also bounds reconnect attempts
/// per individual tool call.
const PROXY_MAX_RETRY_ATTEMPTS: u32 = 3;

/// Backoff between proxy retries, also used as the post-reconnect settle delay.
const PROXY_RETRY_BACKOFF_MS: u64 = 500;

/// How long a request may wait for an on-demand connect (after an idle-close, or
/// while serve is still starting) before falling back to the retry/backoff path.
///
/// Sized for a scale-to-zero host: the remote's ingress *holds* the request while
/// it activates a suspended replica, so the connect itself can legitimately take
/// several seconds. Waiting here is strictly better than returning "reconnecting"
/// on the first call after every idle period.
const PROXY_CONNECT_WAIT_MS: u64 = 20_000;

/// How long `await_peer_bounded` still waits after a definitive on-demand
/// connect refusal, instead of returning immediately or polling out the rest
/// of `PROXY_CONNECT_WAIT_MS`.
///
/// Sized to cover `run_mcp_client`'s own disconnect/reconnect cycle
/// (`reconnect::INTERVAL_SECS`, ~3s) plus margin for the ~100ms synthetic-
/// disconnect delay and `connect_to_serve`'s own latency — so a serve that is
/// merely mid-restart still recovers transparently within this window,
/// exactly as it did before the refusal short-circuit existed, while a
/// genuinely-down serve is still bounded well under the full ~20s budget.
const CONNECT_REFUSAL_GRACE: std::time::Duration =
    std::time::Duration::from_millis(reconnect::INTERVAL_SECS * 1_000 + 1_000);

/// Record a definitive on-demand connect refusal: wake any `await_peer_bounded`
/// callers immediately (via `connect_failed`) instead of leaving them to poll
/// out their full budget for a refusal that is already known, then seed a
/// synthetic disconnect so `run_mcp_client`'s own disconnect/reconnect cycle
/// picks it up. A genuinely slow wake never reaches this function — it
/// resolves via the `Ok` branch in the caller once the peer slot fills in —
/// so this does not shorten a legitimate scale-to-zero wake, only a refusal.
///
/// Pulled out of `run_mcp_client`'s `connect_request_rx` arm so the one line
/// that makes `await_peer_bounded`'s refusal short-circuit real in production
/// is covered by a test that calls this function directly, not only by tests
/// that call `connect_failed.notify_waiters()` themselves in isolation —
/// those pin how `await_peer_bounded` *reacts* to a notification, but nothing
/// previously pinned that this call site still *fires* one: deleting this
/// function's body left the full suite green.
pub(crate) fn note_connect_failure(
    connect_failed: &tokio::sync::Notify,
    disconnect_tx: &tokio::sync::mpsc::Sender<()>,
) {
    connect_failed.notify_waiters();
    let tx = disconnect_tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let _ = tx.send(()).await;
    });
}

/// Heuristic: does this error message describe a transport-level failure
/// (broken TCP, server gone, stale keep-alive, stale session) that warrants
/// a forced reconnect + retry, as opposed to a real tool-level error that
/// the caller should see?
fn is_transport_error_msg(msg: &str) -> bool {
    msg.contains("Transport send error")
        || msg.contains("error sending request")
        || msg.contains("Transport error")
        || msg.contains("connection closed")
        || msg.contains("error decoding response body")
        || msg.contains("Session not found")
        || msg.contains("404")
}

/// Reconnect-related constants for the MCP proxy.
pub(crate) mod reconnect {
    /// How long to wait between reconnect attempts.
    pub const INTERVAL_SECS: u64 = 3;
    /// Maximum total time to spend trying to reconnect before giving up.
    pub const MAX_DURATION_SECS: u64 = 300; // 5 minutes
}

/// Record the current instant as the proxy's most recent activity.
pub(crate) fn mark_proxy_activity(last_activity: &Arc<Mutex<std::time::Instant>>) {
    if let Ok(mut slot) = last_activity.lock() {
        *slot = std::time::Instant::now();
    }
}

/// Resolve the MCP proxy idle-disconnect window: explicit value → env var →
/// `DEFAULT_MCP_PROXY_IDLE_DISCONNECT_SECS`. Mirrors how `run_serve` resolves its
/// own `idle_suspend_secs`. `0` means "never idle-disconnect".
pub(crate) fn resolve_proxy_idle_disconnect_secs(explicit: Option<u64>) -> u64 {
    explicit
        .or_else(|| {
            std::env::var(crate::constants::MCP_PROXY_IDLE_DISCONNECT_SECS_ENV)
                .ok()
                .and_then(|s| s.trim().parse().ok())
        })
        .unwrap_or(crate::constants::DEFAULT_MCP_PROXY_IDLE_DISCONNECT_SECS)
}

/// Has the proxy been idle long enough to close its connection to serve?
///
/// `threshold_secs == 0` disables idle-disconnect, so this always returns false.
/// `now` is a parameter (rather than read from the clock) purely so this is unit
/// testable without sleeping.
pub(crate) fn is_idle(
    last_activity: std::time::Instant,
    threshold_secs: u64,
    now: std::time::Instant,
) -> bool {
    if threshold_secs == 0 {
        return false;
    }
    now.saturating_duration_since(last_activity).as_secs() >= threshold_secs
}

impl ServerHandler for McpProxyService {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("codesearch", env!("CARGO_PKG_VERSION"))
                    .with_title("codesearch (serve proxy)"),
            )
            .with_instructions(
                "Proxy to a running codesearch serve hub. All tool calls are forwarded to the hub.",
            )
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _cx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let _in_flight = InFlightGuard::new(&self.in_flight);
        let mut last_err: Option<String> = None;
        for attempt in 0..PROXY_MAX_RETRY_ATTEMPTS {
            let peer = self.peer.read().await.clone();
            match peer {
                Some(p) => match p.list_tools(request.clone()).await {
                    Ok(r) => {
                        self.mark_activity();
                        return Ok(r);
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if !is_transport_error_msg(&msg) || attempt >= PROXY_MAX_RETRY_ATTEMPTS - 1
                        {
                            return Err(McpError::internal_error(msg, None));
                        }
                        tracing::warn!(
                            "list_tools attempt {}/{} failed (transport): {} — forcing reconnect",
                            attempt + 1,
                            PROXY_MAX_RETRY_ATTEMPTS,
                            msg
                        );
                        last_err = Some(msg);
                        self.force_reconnect().await;
                    }
                },
                None => {
                    // Empty peer slot: either a deliberate idle-close or a real
                    // outage. `try_on_demand_connect` asks the main loop to
                    // connect *now* rather than waiting for the failure-path
                    // reconnect cadence to notice, bounded so we still fall back
                    // to the ordinary retry/backoff below if it doesn't land.
                    if attempt == 0 && self.try_on_demand_connect().await {
                        continue;
                    }
                    if attempt < PROXY_MAX_RETRY_ATTEMPTS - 1 {
                        tokio::time::sleep(std::time::Duration::from_millis(
                            PROXY_RETRY_BACKOFF_MS,
                        ))
                        .await;
                        continue;
                    }
                    return Err(McpError::internal_error(
                        "codesearch serve is reconnecting — please retry in a moment".to_string(),
                        None,
                    ));
                }
            }
        }
        Err(McpError::internal_error(
            last_err.unwrap_or_else(|| "transport error after retries".to_string()),
            None,
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _cx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let _in_flight = InFlightGuard::new(&self.in_flight);
        let mut last_err: Option<String> = None;
        for attempt in 0..PROXY_MAX_RETRY_ATTEMPTS {
            let peer = self.peer.read().await.clone();
            match peer {
                Some(p) => match p.call_tool(request.clone()).await {
                    Ok(r) => {
                        self.mark_activity();
                        return Ok(r);
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if !is_transport_error_msg(&msg) || attempt >= PROXY_MAX_RETRY_ATTEMPTS - 1
                        {
                            return Err(McpError::internal_error(msg, None));
                        }
                        tracing::warn!(
                            "call_tool('{}') attempt {}/{} failed (transport): {} — forcing reconnect",
                            request.name,
                            attempt + 1,
                            PROXY_MAX_RETRY_ATTEMPTS,
                            msg
                        );
                        last_err = Some(msg);
                        self.force_reconnect().await;
                    }
                },
                None => {
                    // Empty peer slot: either a deliberate idle-close or a real
                    // outage. `try_on_demand_connect` asks the main loop to
                    // connect *now* rather than waiting for the failure-path
                    // reconnect cadence to notice, bounded so we still fall back
                    // to the ordinary retry/backoff below if it doesn't land.
                    if attempt == 0 && self.try_on_demand_connect().await {
                        continue;
                    }
                    if attempt < PROXY_MAX_RETRY_ATTEMPTS - 1 {
                        tokio::time::sleep(std::time::Duration::from_millis(
                            PROXY_RETRY_BACKOFF_MS,
                        ))
                        .await;
                        continue;
                    }
                    return Err(McpError::internal_error(
                        "codesearch serve is reconnecting — please retry in a moment".to_string(),
                        None,
                    ));
                }
            }
        }
        Err(McpError::internal_error(
            last_err.unwrap_or_else(|| "transport error after retries".to_string()),
            None,
        ))
    }
}

#[cfg(test)]
#[path = "proxy_idle_tests.rs"]
mod proxy_idle_tests;

/// Unit tests for `await_peer_bounded`'s refusal clamp and the
/// `note_connect_failure` call site that fires it in production, isolated
/// from the full `run_mcp_client` loop by parameterizing the wait budget (and,
/// for the clamp itself, the refusal-grace window) so these run in
/// milliseconds instead of the real `PROXY_CONNECT_WAIT_MS` (~20s) or
/// `reconnect::INTERVAL_SECS` (~3s).
#[cfg(test)]
#[path = "await_peer_tests.rs"]
mod await_peer_tests;
