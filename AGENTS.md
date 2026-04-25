# AGENTS.md — `feature/mcp-multi-repo`

PR is in finishing phase. The remaining work is one bug-fix completion, regression
tests, and PR hygiene. Read top to bottom — every section below has concrete
acceptance criteria.

---

## Status

**Branch:** `feature/mcp-multi-repo`
**Local HEAD:** `e622c7d` — same as `origin/feature/mcp-multi-repo`
**Working tree:** uncommitted changes in `src/mcp/mod.rs` and
`src/fts/tantivy_store.rs` from a regex-search fix that needs completion (see TODO
1 below).
**Tests:** 304 passed / 0 failed (12 ignored) under `cargo test --lib`. Clippy clean.

The features that were planned for this branch are all done and committed:
multi-repo path prefixing with alias-aware dedup, prefix_path_for_ctx refactored
into `MultiStoreContext::prefix_result_path` method, refresh of search-tool
description with regex/phrase guidance, version-bump pre-commit hook with build
guard, copy-to-common version-mismatch protection. Do not redo any of that.

---

## TODO — in order, with acceptance criteria

### 1. Complete the regex-search fix (incomplete in working tree)

**The current uncommitted diff** changes `literal_search` so that
`regex=true` no longer calls `fts_store.search_regex` (which used Tantivy's
`RegexQuery` against tokenized terms — useless for code patterns with `_`,
`::`, punctuation). Instead it uses BM25 for candidate selection, then
post-filters with the actual regex on raw `chunk.content`. This handles many
real cases (snake_case identifiers, namespaces, generics with content words).

**However, the fix has a silent-failure path** that empirical testing
against the codesearch.git index has confirmed:

When the regex contains **no tokenizable words** — only regex syntax such as
`\bfn\s+\w+`, `^[A-Z]\w+`, `\w+_\w+`, `[A-Z]+_[A-Z]+` — BM25 tokenizes the
escapes (`\b`, `\s`, `\w`) into garbage tokens that match nothing in the
indexed code. The BM25 stage returns zero candidates, so the regex
post-filter has nothing to work on, and the response is **empty without an
error** even when the codebase contains thousands of matches.

This is a regression risk: a user who knows regex and types
`\bfn\s+\w+\(` to find all function definitions sees no results and
concludes the tool is broken (or worse, that there are no functions in
their codebase).

**What to add — fallback path for tokenless regexes**

In the `regex=true` branch of `literal_search`, before calling
`fts_store.search(...)`, detect whether the query contains at least one
tokenizable identifier:

```rust
fn regex_has_anchorable_token(pattern: &str) -> bool {
    // Strip regex metacharacters (kept conservative — false positives are
    // safer than false negatives, since the consequence of a false positive
    // here is "BM25 tries first, finds nothing, falls back to scan" — same
    // outcome).
    let stripped: String = pattern.chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    // A run of ≥ 3 alphanumerics is enough to expect a real BM25 token.
    stripped.chars().fold((0usize, false), |(run, found), c| {
        if c.is_alphanumeric() || c == '_' {
            let new_run = run + 1;
            (new_run, found || new_run >= 3)
        } else {
            (0, found)
        }
    }).1
}
```

If `regex_has_anchorable_token(&request.query)` returns `false`, **skip
BM25** and instead scan all chunks in the relevant store(s) sequentially,
applying the regex to `chunk.content`. This is O(N_chunks × pattern) — for
indexes up to a few hundred thousand chunks this is well below 1 second,
which is acceptable for a query that is by construction not BM25-helpable.

The scan path can use the same chunk iterator as `search` does internally;
the only difference is "match every chunk against the compiled regex"
instead of "rank by BM25". Reuse `match_line_for_literal` for the
per-chunk match.

If you can't reach the chunk iterator without bigger refactoring, an
acceptable interim is to use a high BM25 limit (e.g. `usize::MAX / 2`)
combined with an empty-string BM25 query (or a query of `*` if Tantivy
allows) to retrieve all chunks. Then post-filter. Document this as a
known performance ceiling and file an issue.

**Acceptance — item 1**

- New helper `regex_has_anchorable_token` with these tests in
  `src/mcp/mod.rs` test module:
  - `regex_has_anchorable_token("match_line_for_literal")` → true
  - `regex_has_anchorable_token("Vec<.*>")` → true (`Vec` is anchorable)
  - `regex_has_anchorable_token("HashMap::new")` → true
  - `regex_has_anchorable_token("\\bfn\\s+\\w+")` → false
  - `regex_has_anchorable_token("\\.\\w+\\(\\)")` → false
  - `regex_has_anchorable_token("[A-Z]+_[A-Z]+")` → false (after stripping
    metas, `A`, `Z`, `A`, `Z` are runs of 1)
  - `regex_has_anchorable_token("foo")` → true
  - `regex_has_anchorable_token("")` → false
- New end-to-end behaviour test (using the same in-memory test harness
  other `literal_search` tests use):
  - `test_regex_anchorable_uses_bm25_path` — query
    `match_line_for_literal`, regex=true, assert non-empty results AND
    that the BM25 path was taken (e.g. via a test-only counter or by
    checking response shape).
  - `test_regex_tokenless_uses_scan_path` — query `\bfn\s+\w+`, regex=true,
    assert non-empty results from a corpus that contains `fn name(` lines.
    This is the **regression test for the bug this section fixes**.
  - `test_regex_no_match_returns_empty` — query
    `zzz_definitely_not_in_code`, regex=true, assert empty results (must
    pass for both BM25 and scan paths — corner case where BM25 has zero
    candidates AND scan has zero matches).

### 2. Decide and act on `search_regex` `#[cfg(test)]` marker

`tantivy_store.rs` line ~516 marks `search_regex` as `#[cfg(test)]`. This
is wrong unless the function is actually called from a test. Verify:

```powershell
Select-String -Path "src/**/*.rs" -Pattern "search_regex\b" -Recurse
```

If the only references are the definition itself and zero tests, the
function is dead code. Pick one:

- **Delete it.** Cleanest. The Tantivy `RegexQuery` behaviour is not
  something we depend on, and removing the function removes confusion
  about why it exists.
- **Keep it with a test that documents the tokenizer behaviour.** Add
  `test_tantivy_regex_tokenizes_underscores_destructively` that asserts
  `search_regex("foo_bar", ...)` returns zero results when the corpus
  contains `foo_bar`. This makes the `#[cfg(test)]` legitimate and
  serves as a regression sentinel if Tantivy ever changes the
  tokenizer behaviour.

**Acceptance — item 2**

`cargo clippy --all-targets -- -D warnings` passes with no `dead_code`
warnings on `search_regex`. Either it's gone, or there's a test that
calls it.

### 3. Address (or scope out) the `cli::doctor::tests::test_doctor_no_database` failure

The fix-author's note mentions this test fails under `cargo test --all`
because of "global db-discovery context". Commit `9a6e560`
(`fix(test): isolate db_discovery tests from global repos.json`) fixed
the same class of issue for `db_discovery::*` tests. The same pattern
should apply here: a per-test `tempdir` + env override that points
codesearch's config-discovery at an isolated location.

If fixing it now is too big for this PR, **at minimum** open a GitHub
issue titled "test_doctor_no_database leaks global state — port
9a6e560 pattern" and reference it in the PR description.

**Acceptance — item 3**

Either the test passes under `cargo test --all`, or there is a linked
GitHub issue and a commit with `#[ignore = "tracked in #N"]` on the
test so the suite is green.

### 4. Verify, commit, push, open PR

Once items 1–3 are resolved:

```powershell
cargo test --all                          # must be green (or tracked-ignore on item 3)
cargo clippy --all-targets -- -D warnings # must be clean
git diff --stat                           # spot-check
git add -A
git commit                                # pre-commit hook bumps version + rebuilds
git push origin feature/mcp-multi-repo
```

Suggested commit message:
```
fix(mcp): regex literal search uses raw-content matching with scan fallback

- regex=true no longer relies on Tantivy RegexQuery (which tokenizes and
  fails on _, ::, and most code punctuation).
- BM25 candidate selection + raw-content regex post-filter for queries
  with anchorable tokens.
- Sequential chunk scan + regex match for tokenless regex syntax
  (\bfn\s+\w+, etc.) where BM25 has nothing to anchor on.
- Add regex_has_anchorable_token helper + tests covering both paths.
- Mark search_regex as test-only (or remove) — Tantivy RegexQuery is
  not used in production.
```

Then open the PR against `master`. Include in the PR description:
- The bug that prompted the fix (`resolve_routing` with regex=true
  returning zero hits)
- The two-path approach (BM25-anchored vs scan)
- Pointer to `AGENTS_auto-regex_and_confidence.md` as scope for the
  next branch (it depends on this fix landing first)

**Acceptance — item 4**

PR opened, linked from chat, ready for human review.

---

## Build Rules (reference, do not modify lightly)

- Target dir: `C:\WorkArea\AI\codesearch\target` (set by
  `.cargo/config.toml`).
- **Always DEBUG.** `--release` is forbidden on this branch.
- All edits via MCP filesystem tools. Bash/PowerShell only for `cargo`
  commands.
- The pre-commit hook bumps `Cargo.toml` patch version AND rebuilds
  `target/debug/codesearch.exe` so the binary cannot drift behind the
  manifest. If the hook is skipped (`--no-verify`),
  `copy-to-common.ps1` will refuse to deploy a mismatched binary.

## Code Style (reference)

- `anyhow::Result<T>` for fallible functions. No `.unwrap()` /
  `.expect()` in library code.
- Windows path hygiene: normalize via `crate::cache::normalize_path_str`
  before comparing, prefixing, or stripping paths.
- No `print!` / `println!` / `eprintln!` in `src/mcp/` — enforced by
  `test_mcp_no_raw_stdout_calls`.
- Deterministic tests only. No `sleep`. Use `tokio::sync::Barrier` or
  explicit signals when synchronisation is needed.

## Project Architecture (reference)

`codesearch serve` binds `127.0.0.1:39725` (env override
`CODESEARCH_SERVE_PORT`), exposes `GET /health` and MCP Streamable HTTP
at `/mcp`. `codesearch mcp` (stdio) probes `/health` at startup; on
hit, proxies; on miss, runs in stdio standalone mode. **Note:** the
stdio→HTTP proxy currently does not implement MCP session handshake
correctly (separate issue, see `AGENTS_proxy_session_management.md`
in working tree). Direct HTTP clients (OpenCode `type: remote`)
bypass the proxy and work fine.

**Repos config** at `~/.codesearch/repos.json`:
```json
{ "repos": { "<alias>": "<path>" }, "groups": { "<n>": ["<a>", "<b>"] } }
```

**Tool surface:** `search` / `find` / `explore` / `get_chunk` / `status`.

**Key constants:** `DEFAULT_SERVE_PORT=39725`,
`SERVE_PORT_ENV="CODESEARCH_SERVE_PORT"`, `HEALTH_PATH="/health"`,
`MCP_ENDPOINT_PATH="/mcp"`, `HEALTH_PROBE_TIMEOUT_MS=200`.

**Key files:**
- `src/mcp/mod.rs` — tool handlers, `MultiStoreContext`,
  `prefix_result_path`, `match_line_for_literal`, `literal_search`
- `src/serve/mod.rs` — `ServeState`, `RepoState`, file watchers
- `src/mcp/proxy.rs` — `McpProxy` (currently broken for tool calls,
  see note above)
- `src/cli/mod.rs` — `IndexCommands`
- `src/index/mod.rs` — `add_to_index`, `remove_from_index`
- `src/db_discovery/repos.rs` — `ReposConfig`
- `src/fts/tantivy_store.rs` — BM25 index, `search` / `search_phrase` /
  `search_regex` (the last currently `#[cfg(test)]`, see TODO 2)

## Out of scope for this branch

- Auto-regex promotion + literal low-confidence signalling — captured
  in `AGENTS_auto-regex_and_confidence.md`, do **not** start that
  branch until this PR is merged.
- Stdio MCP proxy session management — captured in
  `AGENTS_proxy_session_management.md`, separate branch after this
  one and the auto-regex one.
