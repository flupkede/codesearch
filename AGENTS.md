# AGENTS.md — `feature/mcp-multi-repo`

Branch is feature-complete and fully tested. Only the PR remains.

---

## Status

**Branch:** `feature/mcp-multi-repo`
**Local = Origin HEAD:** `c7fa12f`
**Working tree:** clean (after this commit)
**Tests:** 358 passed / 0 failed (12 ignored) under `cargo test --all`. Clippy clean.

---

## What was done on this branch

All planned work is committed and pushed. Do not redo any of this.

**Multi-repo infrastructure**
- `MultiStoreContext::prefix_result_path` — alias-aware path prefixing across all
  fan-out handlers; dedup key switched from `chunk_id` to `(alias, chunk_id)`
- `VectorStore::iter_all_chunks()` — chunk iterator for scan-path regex queries
- Repos config (`~/.codesearch/repos.json`) with `repos` + `groups` keys
- `codesearch serve` and `codesearch mcp` with health-probe proxy-or-stdio dispatch
- File watcher + config reload in serve mode
- `codesearch index add/remove` with alias and `--keep-config` flag

**Regex search overhaul** (3 commits)
- Replaced Tantivy `RegexQuery` (useless on tokenized terms) with BM25 candidate
  selection + raw-content regex post-filter for anchorable queries
- Added `regex_has_anchorable_token` detector with `need_separator` flag for both
  leading (`\bimpl`) and trailing (`impl\b`) escape merging
- Added sequential chunk scan fallback for tokenless patterns (`\bfn\s+\w+`,
  `^[A-Z]\w+`, `\w+_\w+`, `[A-Z]+_[A-Z]+`, etc.)
- 11 tests: 8 detector unit tests + 3 end-to-end behaviour tests

**Auto-regex promotion + literal low-confidence**
- `looks_like_code_pattern` detector auto-promotes literal queries with code
  punctuation (`=`, `->`, `::`, `;`, etc.) to regex mode without requiring
  `regex=true` from the caller
- `LiteralSearchResponse` wrapper type — **BREAKING** shape change from bare
  `[{...}]` array to `{"results":[{...}], ...}` object with optional
  `auto_promoted_to_regex`, `note`, `low_confidence`, `suggested_tool` fields
- `compute_literal_low_confidence` signals when results are empty or weak,
  with actionable `suggested_tool` hint for the LLM caller

**Proxy removal**
- Deleted `src/mcp/proxy.rs` (283 lines) — the stdio→HTTP proxy was non-functional
  (no MCP Streamable HTTP session handshake). `codesearch mcp` now runs in
  stdio-standalone mode only. Users who want multi-repo support run
  `codesearch serve` and configure their MCP client with `type: remote`.

**Tooling**
- Pre-commit hook bumps `Cargo.toml` patch version AND rebuilds
  `target/debug/codesearch.exe` so binary cannot drift behind manifest
- `copy-to-common.ps1` refuses to deploy a binary whose version does not match
  `Cargo.toml` (version-mismatch guard)
- `test_doctor_no_database` isolated via `CODESEARCH_REPOS_CONFIG` env var
- Magic `384` replaced with `DEFAULT_EMBEDDING_DIMENSIONS` constant
- `REPOS_CONFIG_ENV` constant added, used in `repos.rs` and `doctor.rs`

**Smoke-tested live** (v0.1.243, codesearch.git index, serve on port 39726):

| Query | Path | Hits |
|---|---|:--:|
| `match_line_for_literal` | BM25 | 5 ✅ |
| `\bfn\s+\w+` | scan | 5 ✅ |
| `\bimpl\s+` | scan | 5 ✅ |
| `^[A-Z]\w+` | scan | 5 ✅ |
| `\w+_\w+` | scan | 5 ✅ |
| `[A-Z]+_[A-Z]+` | scan | 5 ✅ |
| `impl\b` | scan | 5 ✅ |
| `Result\b` | scan | 5 ✅ |
| `match\b` | scan | 5 ✅ |
| `zzz_definitely_not_xyz` | — | 0 ✅ |

---

## TODO — one step

### Open the PR

Push this final docs commit, then open the PR on GitHub:
`feature/mcp-multi-repo` → `master`

**Suggested PR title:**
```
feat(mcp): multi-repo support, regex search overhaul, auto-regex promotion
```

**PR description must include:**

1. **Multi-repo infrastructure** — alias-aware path prefixing, group fan-out,
   `codesearch serve` as persistent HTTP hub, `codesearch mcp` in stdio-standalone
   mode (proxy removed as non-functional).

2. **Regex search** — replaced Tantivy `RegexQuery` with BM25+post-filter for
   anchorable queries, plus sequential scan fallback for tokenless patterns.
   Smoke-test table above demonstrates the fix.

3. **BREAKING CHANGE** — `search(mode="literal")` response shape changed from
   bare JSON array `[{...}]` to object `{"results":[{...}], ...}`. Clients
   parsing the old shape must update to use `response.results`.

4. **Auto-regex promotion** — literal queries with code punctuation are
   auto-promoted to regex mode without requiring `regex=true` from the caller.
   Response carries `auto_promoted_to_regex: true` and a `note` when this fires.

5. **Known limitation** (document, do not fix in this PR): queries of the form
   `identifier\b` followed immediately by non-regex content may still exhibit
   edge-case behaviour in the detector. Rare in practice; tracked for a follow-up.

6. **Future branches** (`AGENTS_auto-regex_and_confidence.md` and
   `AGENTS_proxy_session_management.md` are removed from this branch — those
   plans have been superseded by what landed here).

---

## Build rules (reference)

- Target dir: `C:\WorkArea\AI\codesearch\target` (set by `.cargo/config.toml`)
- **Always DEBUG.** `--release` is forbidden on this branch.
- MCP filesystem tools for edits. Bash/PowerShell only for `cargo` commands.
- Pre-commit hook rebuilds binary on every commit — never use `--no-verify`.

## Key files (reference)

- `src/mcp/mod.rs` — all tool handlers, `MultiStoreContext`,
  `prefix_result_path`, `match_line_for_literal`, `literal_search`,
  `regex_has_anchorable_token`, `looks_like_code_pattern`,
  `compute_literal_low_confidence`
- `src/mcp/types.rs` — `LiteralSearchResponse` and related types
- `src/serve/mod.rs` — `ServeState`, `RepoState`, file watchers
- `src/vectordb/store.rs` — `VectorStore`, `iter_all_chunks`
- `src/fts/tantivy_store.rs` — BM25 index; `search_regex` is `#[cfg(test)]`
- `src/cli/mod.rs` — `IndexCommands`
- `src/db_discovery/repos.rs` — `ReposConfig`
- `src/constants.rs` — `DEFAULT_SERVE_PORT`, `DEFAULT_EMBEDDING_DIMENSIONS`,
  `REPOS_CONFIG_ENV`, and other shared constants
