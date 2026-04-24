# AGENTS_proxy_session_management.md

Scoped instructions for a new branch `feature/proxy-session-management`.
**Do not implement on `feature/mcp-multi-repo` or `feature/auto-regex-confidence`** —
this is a standalone fix for a separate branch, cut from `master` once both prior
PRs are merged.

---

## Context — what is broken

`codesearch mcp` (stdio) running in proxy mode does not work when the client
makes a tool call. Observed behaviour (2026-04-24):

1. MCP client sends `initialize` — proxy answers locally (doesn't forward).
2. MCP client sends `tools/call` — proxy calls `POST /mcp` on serve with
   `Content-Type: application/json` and `Accept: application/json, text/event-stream`,
   but **no session setup and no `Mcp-Session-Id` header**.
3. Serve (using `rmcp::transport::StreamableHttpService`) returns **HTTP 422**
   because the MCP Streamable HTTP spec requires a prior `initialize` exchange
   to obtain a session id before any other method call.
4. `proxy::forward()` treats the 422 as a dead connection, sets
   `self.dead = true`, and returns `dead_session_text` ("codesearch serve is
   no longer reachable") for every subsequent call.

Verified directly: a raw `curl`-style POST of `initialize` to `/mcp` returns
200 with a `mcp-session-id` response header and `text/event-stream` body.
So serve is fine; the proxy does not implement the protocol.

## Known state — read before starting

1. **`src/mcp/proxy.rs` has no session state.** `McpProxy` holds `base_url`,
   `client: reqwest::Client`, and `dead: AtomicBool`. There is no
   `session_id` field and no `initialize_done` gate.

2. **`forward()` hardcodes `"id": 1, "method": "tools/call"`** inside a
   `serde_json::json!` macro. Any real flow needs at minimum: a separate
   `initialize` builder, a monotonic id counter, and header handling for
   the `Mcp-Session-Id` both outbound and inbound.

3. **SSE branch already exists in `forward()`** — it reads `resp.text()`,
   splits by lines, strips `data: ` prefix, parses JSON, extracts `result`.
   That part is fine. It is only the session-establishment that is missing.

4. **`dead` semantics are too aggressive.** Any non-success JSON causes
   `dead=true`, which locks the proxy permanently (until client restart).
   The recovery branch tries to re-probe `/health` but that is a different
   state machine from session recovery. HTTP 4xx/5xx on tool calls should
   not set `dead=true`; a network-level error (connection refused, timeout)
   should.

5. **Workaround is already active.** OpenCode config at
   `~/.config/opencode/opencode.json` has been updated to
   `"type": "remote", "url": "http://127.0.0.1:39725/mcp"`. This bypasses the
   stdio proxy entirely — OpenCode connects directly to serve using its own
   MCP client, which does proper session handshake. **Restoring the stdio
   proxy is only useful if users want automatic fallback to stdio when
   serve is not running.** If we always expect `serve` to be running, the
   stdio proxy can be deleted rather than fixed.

## Decision to make before implementation

Pick one of:

**A. Delete the proxy.** Remove the proxy code path in
`run_mcp_server()` and have `codesearch mcp` always run in stdio-standalone
mode. Users who want multi-repo support run serve and configure their
client to connect directly. Simpler. Smaller maintenance surface.

**B. Fix the proxy properly.** Implement the Streamable HTTP client
properly: session handshake, session-id header, SSE parsing, retry on
session expiry. More work but preserves the "transparent fallback" UX.

This document assumes **B**. If the call is **A**, most of what follows is
irrelevant — just rip out `src/mcp/proxy.rs` and the proxy branch of
`run_mcp_server`, plus update the `instructions` string to reflect
stdio-only mode when serve is not reachable.

## Implementation plan (option B)

### Step 1 — session state

```rust
pub struct McpProxy {
    base_url: String,
    client: reqwest::Client,
    dead: AtomicBool,
    session: tokio::sync::Mutex<SessionState>,
    next_id: AtomicU64,
}

enum SessionState {
    Fresh,               // never initialized
    Active(String),      // session_id from server
    Expired,             // server returned 404/session-gone; re-init on next call
}
```

### Step 2 — session-aware `forward()`

Split into `ensure_session()` + `post_request()`:

- `ensure_session()`: if `SessionState::Fresh` or `::Expired`, do a POST of
  `initialize`. Parse the response header `Mcp-Session-Id`. Parse the SSE
  body for the initialize result (we don't need the result itself, just
  proof it succeeded). Store `SessionState::Active(session_id)`. Also send
  `notifications/initialized` immediately after (server expects it before
  accepting other calls).
- `post_request(method, params)`: allocate a new `id` via `next_id`, build
  the JSON-RPC body, include `Mcp-Session-Id` header, POST. If response is
  404 with a session-gone indication, set `SessionState::Expired` and retry
  once (a single level of recursion or a simple retry loop).

### Step 3 — right-sizing the `dead` flag

Only set `dead=true` on:
- `reqwest::Error` with `.is_connect()` or `.is_timeout()` true
- HTTP 5xx with no retry remaining

Do NOT set `dead=true` on:
- 422, 400, or other 4xx (caller error — return an error to the MCP client
  that indicates the tool call failed, but don't poison the whole proxy)
- session-expired 404 (caller should see a re-initialization transparently)

### Step 4 — tests

- `test_proxy_sends_initialize_before_first_tool_call` — mock server,
  assert that the first two POSTs to `/mcp` are `initialize` and
  `notifications/initialized`, in that order.
- `test_proxy_includes_session_id_header_on_tool_call` — mock server
  returns `Mcp-Session-Id` on initialize, assert subsequent POST carries
  same header.
- `test_proxy_reinitializes_after_session_expiry` — mock server returns
  session-gone once, assert proxy does a fresh initialize and retries the
  tool call successfully.
- `test_proxy_422_does_not_mark_dead` — mock server returns 422 on a tool
  call, assert `is_dead()` stays false and a subsequent call is attempted.
- `test_proxy_connect_refused_marks_dead` — mock that drops connections,
  assert `is_dead()` becomes true.

### Step 5 — SSE parsing

Existing code in the SSE branch already works. Just make sure
`ensure_session()` uses it too when parsing the initialize response.

---

## Acceptance criteria

- `cargo test --all` passes
- `cargo clippy --all-targets -- -D warnings` passes
- Manual smoke: start serve, run `codesearch mcp` with stdin feeding
  initialize + notifications/initialized + tools/call, observe actual tool
  result in stdout (not the dead-session text).
- OpenCode can be reconfigured back to `"type": "local", "command":
  ["codesearch", "mcp"]` and work end-to-end (the current config-level
  workaround becomes redundant).

---

## Out of scope

- Rewriting the proxy to use the full `rmcp` client library instead of
  manual reqwest. That would be cleaner but is a large dependency churn.
  Revisit after this fix lands.
- Auto-starting serve from `codesearch mcp` if it is not running — nice
  to have, but a separate feature.
- Changes to serve's endpoint behaviour. Serve is correct; it is the
  client that is wrong.
