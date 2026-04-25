# AGENTS.md — `feature/mcp-multi-repo`

Active development branch. Three follow-up tasks remain. Pick task 1 first
(smallest, fixes a confirmed bug). Tasks 2 and 3 are independent — order between
them doesn't matter.

---

## Status

**Branch:** `feature/mcp-multi-repo`
**Local + origin HEAD:** `a384adb` (both in sync, all earlier work pushed)

**Already done on this branch — DO NOT REDO ANY OF THIS:**

- Multi-repo path prefixing with alias-aware dedup (`MultiStoreContext::prefix_result_path`)
- Refresh of `search` tool description with regex/phrase guidance
- Pre-commit hook bumps `Cargo.toml` patch version AND rebuilds
  `target/debug/codesearch.exe` (so binary cannot drift)
- `copy-to-common.ps1` refuses to deploy mismatched-version binary
- Regex-search BM25 + raw-content post-filter (commit `af9996f`)
- Regex-search scan fallback for tokenless queries (commit `43ef12c`),
  including detector `regex_has_anchorable_token` and 11 tests
- `search_regex` marked `#[cfg(test)]` (already had a calling test)
- `test_doctor_no_database` isolated via `CODESEARCH_REPOS_CONFIG` env

**Tests:** 315 lib tests pass, clippy clean.

**Live smoke-tested (codesearch.git index, port 39726):** all six previously
failing tokenless regex queries now return ≥ 5 hits each.

---

## Task 1 — Trailing-escape detector fix

### What is broken

After `43ef12c` landed, edge-case sweep found one query pattern that still
returns zero hits:

| Query | Result | Should be |
|---|:--:|---|
| `\bimpl` | 5 hits ✅ | works |
| `impl` | 5 hits ✅ | works |
| **`impl\b`** | **0 hits** ❌ | should match `impl` at word boundaries |
| **`Result\b`** | **0 hits** ❌ | should match `Result` |
| **`match\b`** | **0 hits** ❌ | should match `match` keyword |

### Root cause

`regex_has_anchorable_token` in `src/mcp/mod.rs` already handles **leading**
escapes correctly via the `need_separator` flag: after `\X` or `[...]`, the
next alphanumeric run is not counted because Tantivy's BM25 analyzer merges
the escape content with the following letters into one token (`\bimpl` →
`bimpl`, not `impl`).

The same merging happens in reverse for **trailing** escapes. `impl\b` is
analyzed by Tantivy into a token that mixes `impl` with the `\b` content,
not the bare token `impl`. So when the detector marks `impl\b` as anchorable
(it sees a 3+ alphanumeric run "impl" with no leading escape), the BM25
path is taken, BM25 finds zero candidates because no chunk's tokens match the
merged form, and the regex post-filter never runs.

The detector currently only looks **forward** when it sees an alphanumeric
run reach the threshold. It needs to also look one position past the run end
to detect trailing-merge.

### What to add

In `regex_has_anchorable_token` (in `src/mcp/mod.rs`), modify the branch that
returns `true` when `run >= 3`. Before returning, peek at the next byte. If
that byte is `\` or `[`, the alphanumeric run merges with the following
escape or character class — treat as not anchorable, reset `run`, set
`need_separator = true`, continue scanning.

Sketch (adapt to existing function structure):

```rust
if c.is_alphanumeric() || c == '_' {
    if need_separator {
        // existing logic — DO NOT TOUCH
        i += 1;
        continue;
    }
    run += 1;
    if run >= 3 {
        // Look ahead: is the next byte an escape or class start?
        // If yes, BM25 will merge the run with following content → not anchorable.
        let next_idx = i + 1;
        if next_idx < bytes.len() {
            let next_c = bytes[next_idx] as char;
            if next_c == '\\' || next_c == '[' {
                run = 0;
                need_separator = true;
                i += 1;
                continue;
            }
        }
        return true;
    }
} else {
    run = 0;
    need_separator = false;
}
```

The peek must happen **only when `run >= 3`**, not on every alphanumeric
character. Peeking earlier is wasteful and produces wrong behaviour while
runs are still building toward the threshold.

### Tests for task 1

Append to the existing test module in `src/mcp/mod.rs` (find the existing
`test_regex_has_anchorable_token_*` block and add these next to them):

```rust
#[test]
fn test_regex_has_anchorable_token_trailing_word_boundary() {
    assert!(!regex_has_anchorable_token(r"impl\b"));
    assert!(!regex_has_anchorable_token(r"Result\b"));
    assert!(!regex_has_anchorable_token(r"match\b"));
}

#[test]
fn test_regex_has_anchorable_token_trailing_class() {
    assert!(!regex_has_anchorable_token(r"impl[A-Z]"));
    assert!(!regex_has_anchorable_token(r"foo[abc]+"));
}

#[test]
fn test_regex_has_anchorable_token_trailing_escape_with_clean_run_after() {
    // After the merged trailing escape, if there's a clean run later, that
    // later run can still anchor.
    assert!(regex_has_anchorable_token(r"impl\b\s+function_name"));
    //                                              ^^^^^^^^^^^^^ anchorable
}

#[test]
fn test_regex_has_anchorable_token_trailing_escape_at_end_only() {
    // Run, then escape, then EOF — not anchorable.
    assert!(!regex_has_anchorable_token(r"impl\s"));
}

#[test]
fn test_regex_has_anchorable_token_both_sides_escaped() {
    // \bimpl\b — leading escape already disqualifies "impl"; trailing
    // doesn't change the answer.
    assert!(!regex_has_anchorable_token(r"\bimpl\b"));
}
```

Plus one end-to-end behaviour test, copying the setup from
`test_regex_tokenless_uses_scan_path` already in the file:

```rust
#[test]
fn test_regex_trailing_escape_uses_scan_path() {
    // Corpus contains "impl Foo for Bar".
    // Query "impl\b" must return ≥ 1 result with score == 0.0 (scan-path marker).
}
```

### Acceptance for task 1

- All 8 existing detector tests still pass (zero regressions on leading-escape).
- All 3 existing behaviour tests still pass.
- 5 new detector tests + 1 new behaviour test pass.
- Smoke test against codesearch.git: `impl\b`, `Result\b`, `match\b`,
  `impl[A-Z]` each return ≥ 1 hit.
- `cargo clippy --all-targets -- -D warnings` clean.

### Suggested commit message for task 1

```
fix(mcp): trailing-escape regex queries route to scan path

Patterns like impl\b, Result\b, match\b previously returned zero results
because the anchorable-token detector did not look ahead past a 3+
alphanumeric run. BM25 receives the raw query and merges the escape into
the identifier token, producing zero candidates.

Extend regex_has_anchorable_token to peek one position past a run of
length ≥ 3. If the next byte is \ or [, the run merges with following
content and is not counted as anchorable, routing the query to the scan
path instead.

5 new detector tests + 1 new behaviour test. Existing 8 detector tests
+ 3 behaviour tests pass unchanged.
```

---

## Task 2 — Auto-regex promotion + literal low-confidence

### Why

Two related improvements that reduce silent-failure modes for literal search:

1. **Auto-regex promotion.** When a user writes `search(mode="literal", query="foo = null")`
   without setting `regex=true`, BM25 tokenizes `=` and `null` as separate
   terms, producing noisy results. Detect code-pattern punctuation in the
   query and automatically apply `regex::escape` (with whitespace relaxation)
   + enable regex mode. The user gets correct results without having to know
   the BM25 quirk.

2. **Low-confidence signal for literal search.** `SemanticSearchResponse`
   already signals `low_confidence + suggested_tool` when RRF top-score is
   weak. Literal search has no equivalent. When BM25 returns zero hits or
   weak results, the LLM sees an empty result with no guidance and falls
   back to external `rg` (which may not be installed). Mirror the
   semantic-search signalling into literal-search responses.

### Known state — verify before starting

1. **`LiteralSearchResponse` does not currently exist.** `literal_search`
   returns either a bare `Vec<LiteralSearchResultItem>` JSON array (default)
   or a grep-format string. Introducing `LiteralSearchResponse { results, ... }`
   changes JSON shape from `[{...}]` to `{"results":[{...}]}` — this is a
   **breaking change** for any client that parses the array directly.
   Search the repo for parsers expecting bare arrays:

   ```powershell
   git grep -n "literal_search\|LiteralSearchResultItem" -- src/ tests/
   ```

   Update every site to use the new `LiteralSearchResponse.results` field.
   Document the shape change prominently in commit message and PR
   description.

2. **Grep-format output cannot carry JSON fields.** When `format = "grep"`,
   output is plain text. Use `# ` comment lines for `auto_promoted_to_regex`
   and `low_confidence` markers (standard grep convention; line-splitting
   consumers skip `#`-prefixed lines).

3. **The BM25 low-confidence threshold must be instrumented before chosen.**
   BM25 absolute scores depend on corpus, field length, tokenizer config —
   a hardcoded value (e.g. `0.5`) is a guess until measured. Add an
   instrumentation-only commit first, run against real traffic, **then**
   set the constant to a calibrated value.

4. **The parent commits in this branch already updated the search description**
   to include sub-mode guidance for `regex=true` / `phrase=true`. The
   auto-promotion logic in this task is a programmatic backstop for when
   the LLM ignores the description. No further description changes needed.

### Auto-regex promotion implementation

Add a pure detector in `src/mcp/mod.rs` near `match_line_for_literal`:

```rust
/// Returns true when a literal-search query looks like a code pattern whose
/// punctuation would be destroyed by BM25 tokenization.
///
/// Triggers on:
/// - Multi-char operators: ->, =>, ::, !=, ==, <=, >=, &&, ||, <<, >>
/// - Space-surrounded single operators: " = ", " < ", " > "
/// - Statement endings: trailing `;` or `{`
/// - ≥ 2 angle/square bracket characters: `Vec<T>`, `[0]`
///
/// Does NOT trigger on:
/// - Plain identifiers: "ActivitiesListModelResponse", "foo_bar"
/// - Dotted paths: "foo.bar", "System.Console"
/// - Single parens alone: "(error)" — parens are not in the bracket set
fn looks_like_code_pattern(query: &str) -> bool {
    const MULTI_OPS: &[&str] = &[
        "->", "=>", "::", "!=", "==", "<=", ">=", "&&", "||", "<<", ">>",
    ];
    if MULTI_OPS.iter().any(|op| query.contains(op)) {
        return true;
    }
    const SPACED_OPS: &[&str] = &[" = ", " < ", " > "];
    if SPACED_OPS.iter().any(|op| query.contains(op)) {
        return true;
    }
    let trimmed = query.trim();
    if trimmed.ends_with(';') || trimmed.ends_with('{') {
        return true;
    }
    let bracket_count = query
        .chars()
        .filter(|c| matches!(c, '<' | '>' | '[' | ']'))
        .count();
    bracket_count >= 2
}
```

In the `literal_search` handler, near the top after extracting `request`:

```rust
let user_set_regex  = request.regex.unwrap_or(false);
let user_set_phrase = request.phrase.unwrap_or(false);
let auto_promoted   = !user_set_regex
    && !user_set_phrase
    && looks_like_code_pattern(&request.query);

let (effective_query, effective_regex) = if auto_promoted {
    let escaped = regex::escape(&request.query);
    // Relax escaped spaces to \s+ so "foo = null" → "foo\s+=\s+null"
    let relaxed = escaped.replace(r"\ ", r"\s+");
    (relaxed, true)
} else {
    (request.query.clone(), user_set_regex)
};
```

Replace every downstream use of `request.query` with `effective_query` and
`request.regex` with `effective_regex` inside the rest of the handler.

### Wrapping response type

Add to `src/mcp/types.rs`:

```rust
/// Response from `search(mode="literal")`.
///
/// Replaces the previous bare `Vec<LiteralSearchResultItem>` JSON array.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiteralSearchResponse {
    pub results: Vec<LiteralSearchResultItem>,

    /// True when codesearch auto-escaped the query and enabled regex mode
    /// because the original query contained code-like punctuation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_promoted_to_regex: Option<bool>,

    /// Actionable note for the LLM caller (present iff auto_promoted_to_regex
    /// or low_confidence is set).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,

    /// True when results are empty or top BM25 score is below threshold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub low_confidence: Option<bool>,

    /// Suggested next tool when low_confidence is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_tool: Option<String>,
}
```

At the end of `literal_search`, wrap the items into the new type with the
combined note logic — see `AGENTS_auto-regex_and_confidence.md` (in repo
history if needed) for the exact wrapping code, or just write it
straightforward: pick the strongest signal first (auto-promoted if true,
otherwise low-confidence note, otherwise `None`).

### Low-confidence helper

```rust
fn compute_literal_low_confidence(
    top_score: Option<f32>,
    query: &str,
) -> (Option<bool>, Option<String>) {
    let word_count = query.split_whitespace().count();
    let has_code_chars = query.chars().any(|c| "{}[]<>=|;:".contains(c));
    let is_natural_language = word_count >= 3 && !has_code_chars;

    let suggest_semantic = "search with mode='semantic'";
    let suggest_regex    = "search with mode='literal' and regex=true";
    let suggest_find     = "find with kind='definition' or kind='usages'";

    match top_score {
        Some(score) if score < LITERAL_LOW_CONFIDENCE_BM25 => {
            let hint = if is_natural_language { suggest_semantic } else { suggest_find };
            (Some(true), Some(hint.to_string()))
        }
        None => {
            let hint = if is_natural_language { suggest_semantic } else { suggest_regex };
            (Some(true), Some(hint.to_string()))
        }
        Some(_) => (None, None),
    }
}
```

Set the threshold initially to a never-fires value:

```rust
/// BM25 score threshold for low-confidence signalling in literal search.
/// IMPORTANT: this value is unvalidated. It is set to f32::MAX (never fires)
/// until real query scores have been collected. Update after reviewing
/// `RUST_LOG=codesearch::literal_confidence=debug` output.
const LITERAL_LOW_CONFIDENCE_BM25: f32 = f32::MAX;
```

Add instrumentation in `literal_search`:

```rust
if let Some(top) = items.first() {
    tracing::debug!(
        target: "codesearch::literal_confidence",
        query = %request.query,
        top_bm25_score = top.score,
        result_count = items.len(),
        "literal_search score sample"
    );
}
```

### Grep-format with comment markers

When `format == "grep"`, prepend `# ` comment lines for active signals:

```rust
let output = if output_format == "grep" {
    let mut lines: Vec<String> = Vec::new();
    if response.auto_promoted_to_regex == Some(true) {
        lines.push("# auto-promoted to regex mode (query contained code-like punctuation)".to_string());
    }
    if response.low_confidence == Some(true) {
        if let Some(ref hint) = response.suggested_tool {
            lines.push(format!("# low confidence — consider: {}", hint));
        }
    }
    for item in &response.results {
        lines.push(format!("{}:{}:{}", item.path, item.start_line, item.snippet));
    }
    lines.join("\n")
} else {
    serde_json::to_string(&response).unwrap_or_else(|_| "{}".to_string())
};
```

### Tests for task 2

In `src/mcp/mod.rs` test module:

**Detector unit tests:**
- `test_looks_like_code_pattern_assignment` — `"foo = null"`, `"x = 42"` → true
- `test_looks_like_code_pattern_arrow` — `"foo->bar"`, `"x => y"` → true
- `test_looks_like_code_pattern_namespace` — `"std::string"`, `"a::b::c"` → true
- `test_looks_like_code_pattern_generics` — `"Vec<T>"`, `"HashMap<K, V>"` → true
- `test_looks_like_code_pattern_statement_end` — `"return x;"`, `"if (x) {"` → true
- `test_looks_like_code_pattern_plain_identifier_false` —
  `"ActivitiesListModelResponse"`, `"foo_bar"` → false
- `test_looks_like_code_pattern_dotted_path_false` — `"foo.bar"`,
  `"System.Console"` → false
- `test_looks_like_code_pattern_empty_false` — `""` → false

**Auto-promotion behaviour tests:**
- `test_auto_promotion_escapes_and_relaxes_spaces` — `"foo = null"` →
  effective_query is `r"foo\s+=\s+null"`
- `test_auto_promoted_skipped_when_user_sets_regex` — `regex=true` →
  `auto_promoted=false`, query unchanged
- `test_auto_promoted_skipped_when_user_sets_phrase` — `phrase=true` → same
- `test_literal_search_response_shape_json` — JSON starts with `{`,
  contains `"results":[`, does NOT start with `[`
- `test_literal_search_response_carries_note_when_promoted` — promoted call
  → `auto_promoted_to_regex: Some(true)` + non-empty `note`
- `test_literal_search_response_omits_fields_when_not_promoted` — plain call
  → JSON does not contain `"auto_promoted_to_regex"` or `"note"` keys
- `test_grep_format_includes_comment_when_promoted` — grep output starts with
  `# auto-promoted`
- `test_grep_format_no_comment_when_plain` — plain query in grep format → output
  does not start with `#`

**Low-confidence tests** (use `top_score = Some(LITERAL_LOW_CONFIDENCE_BM25 - 0.001)`
to force trigger without depending on the actual constant value):
- `test_literal_lc_natural_language_zero_results` — `"how do we handle auth"`,
  `top_score = None` → suggests `semantic`
- `test_literal_lc_identifier_zero_results` — `"CodesearchService"`,
  `top_score = None` → suggests `regex`
- `test_literal_lc_code_pattern_zero_results` — `"foo = null"`,
  `top_score = None` → suggests `regex`
- `test_literal_lc_natural_language_weak_score` — natural-language query,
  weak score → suggests `semantic`
- `test_literal_lc_identifier_weak_score` — identifier query, weak score →
  suggests `find`
- `test_literal_lc_threshold_uses_strictly_less_than` —
  `top_score = Some(LITERAL_LOW_CONFIDENCE_BM25)` → `(None, None)`
- `test_literal_lc_high_score_returns_none` — `top_score = Some(f32::MAX)`
  → `(None, None)`
- `test_literal_response_json_has_lc_fields` — `low_confidence = Some(true)`
  → JSON contains `"low_confidence":true` and `"suggested_tool"`
- `test_literal_response_json_omits_lc_fields_when_none` — null values →
  keys absent from JSON

### Commit structure for task 2

Five commits, each green:

1. `feat(mcp): add looks_like_code_pattern detector (pure fn + tests)`
2. `feat(mcp): auto-promote literal queries with code punctuation to regex mode`
   — introduce `LiteralSearchResponse`, update all consumers of bare-array
   response (this is the **breaking change** commit; document it loudly).
3. `feat(mcp): instrument literal search BM25 scores for threshold calibration`
   — `LITERAL_LOW_CONFIDENCE_BM25 = f32::MAX` (never fires yet)
4. `feat(mcp): add compute_literal_low_confidence + wire into LiteralSearchResponse`
5. `chore(mcp): set LITERAL_LOW_CONFIDENCE_BM25 to calibrated value` — fill in
   after reviewing debug logs; commit message must include observed score
   distribution (e.g. "p50=1.8, p10=0.6, chose threshold=0.5 to flag bottom
   ~10% of hits")

### Acceptance for task 2

- `cargo test --all` passes (8 detector tests + 8 auto-promotion tests +
  9 low-confidence tests + all existing tests).
- `cargo clippy --all-targets -- -D warnings` clean.
- Manual smoke: `search(mode="literal", query="ActivitiesListModelResponse = null")`
  against a real codebase returns JSON with `"auto_promoted_to_regex": true`,
  `"note"` explaining the promotion, and `"results"` containing actual
  assignment lines.
- All sites that previously expected a bare JSON array updated.

### Out of scope for task 2

- Auto-promotion for **semantic** search — embeddings are not harmed by punctuation.
- Rewriting the Tantivy analyzer to tokenize code differently — risky upstream
  concern.
- Changes to `src/cli/mod.rs`, `src/serve/mod.rs`, `README.md` — task 2 is
  `src/mcp/mod.rs` + `src/mcp/types.rs` only.

---

## Task 3 — Stdio MCP proxy session handshake

### Why this is here

`codesearch mcp` (stdio proxy mode) does not implement MCP Streamable HTTP
session handshake. When a client sends a `tools/call` request, the proxy
posts to `/mcp` without `Mcp-Session-Id`, serve returns HTTP 422
(Unprocessable Entity), and the proxy marks itself dead → all subsequent
calls fail with "codesearch serve is no longer reachable".

Current workaround: OpenCode connects directly via `type: "remote"` to
`http://127.0.0.1:39725/mcp`, which uses OpenCode's own MCP client (which
does the handshake correctly). This works fine and is the documented setup.

### Decision needed before starting

This task may be obsolete. If everyone is using `type: remote` (direct HTTP),
the stdio proxy isn't needed and the safest fix is to **delete it**. If
keeping the stdio proxy as a fallback for "serve not running" is valuable,
fix it properly.

Pick one before writing code:

**A. Delete the proxy.** Remove `src/mcp/proxy.rs`. In `run_mcp_server` in
`src/mcp/mod.rs`, remove the proxy branch — `codesearch mcp` always runs in
stdio-standalone mode (just the local DB). Users who want multi-repo run
serve and configure their client with `type: remote`. Smaller maintenance
surface.

**B. Fix the proxy properly.** Implement Streamable HTTP client correctly:
session handshake, session-id header, retry on session expiry, separate
network errors from protocol errors in the dead-flag logic.

**Recommendation:** A. The "transparent fallback" UX is rarely what users
want — they either run serve or they don't. If they don't, current
proxy-mode behaviour is broken anyway. Keeping working code that nobody
exercises is a worse outcome than removing it.

If you pick **A**, the rest of this section is mostly unnecessary; just
delete `src/mcp/proxy.rs`, remove the call site, update `instructions` to
not advertise proxy mode, and add a test confirming `codesearch mcp` runs
standalone correctly when serve is not reachable.

If you pick **B**, continue below.

### Option B implementation

#### Step 1 — session state

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
    Expired,             // server returned 404; re-init on next call
}
```

#### Step 2 — split `forward()`

Into `ensure_session()` + `post_request()`:

- `ensure_session()`: if `Fresh` or `Expired`, POST `initialize`. Parse
  `Mcp-Session-Id` from response header. Parse SSE body for the
  `initialize` result (proof it succeeded, result content not needed).
  Store `Active(session_id)`. Then send `notifications/initialized` (server
  expects it before accepting other calls).
- `post_request(method, params)`: allocate id via `next_id`, build JSON-RPC
  body, include `Mcp-Session-Id` header, POST. On session-expired 404, set
  `Expired` and retry once.

#### Step 3 — right-size `dead`

Only set `dead=true` on:
- `reqwest::Error` with `.is_connect()` or `.is_timeout()` true
- HTTP 5xx with no retry remaining

Do NOT set `dead=true` on:
- 422, 400, other 4xx (caller errors — return error to MCP client, but
  don't poison the proxy)
- Session-expired 404 (caller sees a transparent re-init)

### Tests for task 3 (option B only)

- `test_proxy_sends_initialize_before_first_tool_call` — mock server,
  assert first two POSTs are `initialize` then `notifications/initialized`.
- `test_proxy_includes_session_id_header_on_tool_call` — mock returns
  session-id on initialize, subsequent POST carries same header.
- `test_proxy_reinitializes_after_session_expiry` — mock returns
  session-gone once, proxy does fresh initialize and retries successfully.
- `test_proxy_422_does_not_mark_dead` — 422 on tool call, `is_dead()`
  stays false.
- `test_proxy_connect_refused_marks_dead` — drops connections,
  `is_dead()` becomes true.

### Acceptance for task 3

- `cargo test --all` passes.
- `cargo clippy --all-targets -- -D warnings` clean.
- For option A: `codesearch mcp` tested standalone works when serve is not
  running.
- For option B: manual smoke — start serve, run `codesearch mcp` with
  stdin feeding initialize + notifications/initialized + tools/call,
  observe actual tool result in stdout. Reconfigure OpenCode back to
  `"type": "local"` and verify end-to-end.

### Out of scope for task 3

- Rewriting the proxy to use the full `rmcp` client library instead of
  manual `reqwest`. Cleaner but a large dependency churn — separate task.
- Auto-starting serve from `codesearch mcp` if not running.

---

## Final hygiene — after all 3 tasks land

1. `cargo test --all` green.
2. `cargo clippy --all-targets -- -D warnings` clean.
3. `git push origin feature/mcp-multi-repo`.
4. Open PR against `master`. PR description must list all three tasks
   completed (1: trailing-escape, 2: auto-regex + low-confidence, 3:
   proxy decision A or B), with the breaking change in task 2 (LiteralSearchResponse
   shape) called out prominently in a separate paragraph.

---

## Build Rules (reference)

- Target dir `C:\WorkArea\AI\codesearch\target` set by `.cargo/config.toml`.
- **Always DEBUG.** `--release` is forbidden on this branch.
- All edits via MCP filesystem tools. Bash/PowerShell only for cargo.
- Pre-commit hook bumps `Cargo.toml` patch version AND rebuilds
  `target/debug/codesearch.exe`. If hook is skipped (`--no-verify`),
  `copy-to-common.ps1` will refuse to deploy a mismatched binary.

## Code Style (reference)

- `anyhow::Result<T>` for fallible functions. No `.unwrap()` / `.expect()`
  in library code.
- Windows path hygiene: normalize via `crate::cache::normalize_path_str`
  before comparing, prefixing, or stripping paths.
- No `print!` / `println!` / `eprintln!` in `src/mcp/` — enforced by
  `test_mcp_no_raw_stdout_calls`.
- Deterministic tests only. No `sleep`. Use `tokio::sync::Barrier` or
  explicit signals for synchronisation.

## Project Architecture (reference)

`codesearch serve` binds `127.0.0.1:39725` (env override
`CODESEARCH_SERVE_PORT`), exposes `GET /health` and MCP Streamable HTTP at
`/mcp`. `codesearch mcp` (stdio) probes `/health` at startup; on hit it
proxies to serve (broken — see task 3), on miss it runs stdio standalone.
Direct HTTP clients with `type: remote` work fine.

**Tool surface:** `search` / `find` / `explore` / `get_chunk` / `status`.

**Repos config** at `~/.codesearch/repos.json`:
```json
{ "repos": { "<alias>": "<path>" }, "groups": { "<n>": ["<a>", "<b>"] } }
```

**Key files:**
- `src/mcp/mod.rs` — tool handlers, `MultiStoreContext`, `prefix_result_path`,
  `match_line_for_literal`, `literal_search`, `regex_has_anchorable_token`
- `src/mcp/types.rs` — request/response types (where `LiteralSearchResponse`
  goes for task 2)
- `src/mcp/proxy.rs` — `McpProxy` (broken; see task 3 — may be deleted)
- `src/serve/mod.rs` — `ServeState`, file watchers
- `src/cli/mod.rs` — `IndexCommands`
- `src/index/mod.rs` — `add_to_index`, `remove_from_index`
- `src/db_discovery/repos.rs` — `ReposConfig`
- `src/fts/tantivy_store.rs` — BM25 index; `search_regex` is `#[cfg(test)]`,
  do not call from production
- `src/vectordb/store.rs` — `VectorStore`, `iter_all_chunks` (scan-path entry)
