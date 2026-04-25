# AGENTS.md — `feature/mcp-multi-repo`

Branch is feature-complete. Only PR hygiene remains.

---

## Status

**Branch:** `feature/mcp-multi-repo`
**Local HEAD:** `43ef12c` — 2 commits ahead of `origin/feature/mcp-multi-repo`,
not pushed
**Working tree:** clean (after this AGENTS.md commit)

The two unpushed commits:

- `af9996f` — `fix(mcp): regex search uses BM25 candidates + raw-content post-filter`
  Replaced Tantivy `RegexQuery` with BM25-then-regex-post-filter for queries with
  identifier-like content. Marked `search_regex` as `#[cfg(test)]`. Fixed
  `test_doctor_no_database` global-state leak.
- `43ef12c` — `fix(mcp): scan fallback for tokenless regex queries`
  Added `regex_has_anchorable_token` detector and a sequential scan path for
  patterns like `\bfn\s+\w+`, `^[A-Z]\w+`, `\w+_\w+` that BM25 cannot anchor on.
  Added `VectorStore::iter_all_chunks()` for the scan path. 11 new tests (8
  detector, 3 end-to-end behaviour).

**Tests:** 315 passed / 0 failed (12 ignored) under `cargo test --lib`. Clippy
clean under `-D warnings`.

**Smoke-tested live** against codesearch.git index (v0.1.237 binary, serve on
port 39726). All six previously-failing tokenless regex queries now return ≥ 5
hits each:

| Query | Hits | Path |
|---|:--:|---|
| `match_line_for_literal` | 5 | BM25 (control) |
| `\bfn\s+\w+` | 5 | scan |
| `\bimpl\s+` | 5 | scan |
| `^[A-Z]\w+` | 5 | scan |
| `\w+_\w+` | 5 | scan |
| `[A-Z]+_[A-Z]+` | 5 | scan |
| `zzz_definitely_xyz` (negative) | 0 | scan, correct |

Performance: scan path runs in 74–108 ms against the codesearch.git index
(~5k chunks). BM25 path 20–35 ms. Both well under acceptable thresholds.

---

## TODO — in order

### 1. Push and open PR

```powershell
cargo test --all                          # confirm green (item 3 already isolated test_doctor_no_database)
cargo clippy --all-targets -- -D warnings # confirm clean
git push origin feature/mcp-multi-repo
```

Open PR against `master`. The PR description must include:

- The original bug (regex queries with code patterns silently returning zero
  hits because Tantivy `RegexQuery` runs on tokenized terms).
- The two-path solution (BM25 for queries with anchorable identifiers ≥ 3
  alphanumerics; scan fallback for tokenless regex syntax).
- A pointer to the **known limitation** below and the follow-up branch.
- Pointers to `AGENTS_auto-regex_and_confidence.md` and
  `AGENTS_proxy_session_management.md` as scope for **future** branches, not
  this PR.

---

## Known limitation — needs follow-up branch (not this PR)

### `identifier\b` style queries fail

During post-fix validation, one query pattern was found that still returns zero
results despite matching content existing in the corpus:

| Query | Result |
|---|:--:|
| `\bimpl` | 5 hits ✅ |
| `impl` | 5 hits ✅ |
| **`impl\b`** | **0 hits ❌** |
| **`Result\b`** | **0 hits ❌** |
| **`match\b`** | **0 hits ❌** |
| `\bfn\b` | 5 hits ✅ (incidental — `fn` is < 3 chars, falls to scan path) |

**Root cause.** The detector marks `impl\b` as anchorable (3 alphanumerics →
"impl"), so BM25 path is taken. But BM25 receives the raw query string
`impl\b`, which the Tantivy analyzer tokenizes into something containing `impl`
mixed with `b`, not the bare token `impl`. Zero candidates → regex post-filter
runs on nothing → empty results.

This is the **mirror** of the leading-escape case the detector already handles
via `need_separator`: after a `\X` escape, following alphanumerics merge with
the escape content. The same applies in reverse for trailing escapes — but the
detector currently only looks forward, not backward.

**Why this is not a blocker.** The PR fixes 6/6 of the originally reported
failure cases plus all common code patterns (snake_case, generics, namespaces,
character classes). The trailing-escape pattern was not in the bug report and
was discovered only by exhaustive edge-case sweeping. Users who hit this can
work around by writing `\bimpl\b` (which falls to scan path correctly) or by
removing the trailing `\b`. A clean fix can land in a follow-up branch.

### Tracked in: `AGENTS_trailing_escape_detector.md`

This file is the scope document for branch `feature/regex-trailing-escape`,
**to be cut from master after the current PR merges**. Do not implement on
this branch.

The follow-up should:

1. Extend `regex_has_anchorable_token` to look one position past the end of an
   alphanumeric run for `\X` escapes or `[...]` classes that would back-merge
   with the run. If found, do not count the run.
2. Add tests for `impl\b`, `Result\b`, `match\b` (must return false from
   detector → scan path → real hits).
3. Re-validate all eight existing detector tests still pass unchanged.
4. Live smoke test of `impl\b` returns ≥ 5 hits.

Mechanically: replace the current single-pass forward scan in the detector
with a function that, when it sees a candidate run end (length ≥ 3), looks at
the **next** byte before returning true. If it is `\` or `[`, treat the run
as merged-with-trailing-escape and continue looking instead of returning true.

---

## What is explicitly out of scope for this PR

- **Auto-regex promotion + literal-mode low-confidence signalling** — see
  `AGENTS_auto-regex_and_confidence.md`. Future branch, do not start until
  this PR is merged.
- **Stdio MCP proxy session handshake** — see
  `AGENTS_proxy_session_management.md`. Separate branch after this and the
  auto-regex one.
- **Trailing-escape detector fix** — see "Known limitation" above. Future
  branch, in `AGENTS_trailing_escape_detector.md` (to be created, scope already
  documented above).
- **Performance tuning of the scan path.** Current scan completes in < 110ms
  on codesearch.git (~5k chunks). For substantially larger indexes, profile
  first, optimize second.

---

## Build Rules (reference)

- Target dir `C:\WorkArea\AI\codesearch\target` set by `.cargo/config.toml`.
- **Always DEBUG.** `--release` is forbidden on this branch.
- All edits via MCP filesystem tools. Bash/PowerShell only for cargo commands.
- Pre-commit hook bumps `Cargo.toml` patch version AND rebuilds
  `target/debug/codesearch.exe` so the binary cannot drift behind the manifest.
  If hook is skipped (`--no-verify`), `copy-to-common.ps1` will refuse to
  deploy a mismatched binary.

## Code Style (reference)

- `anyhow::Result<T>` for fallible functions. No `.unwrap()` / `.expect()` in
  library code.
- Windows path hygiene: normalize via `crate::cache::normalize_path_str` before
  comparing, prefixing, or stripping paths.
- No `print!` / `println!` / `eprintln!` in `src/mcp/` — enforced by
  `test_mcp_no_raw_stdout_calls`.
- Deterministic tests only. No `sleep`. Use `tokio::sync::Barrier` or explicit
  signals for synchronisation.

## Project Architecture (reference)

`codesearch serve` binds `127.0.0.1:39725` (env override
`CODESEARCH_SERVE_PORT`), exposes `GET /health` and MCP Streamable HTTP at
`/mcp`. `codesearch mcp` (stdio) probes `/health` at startup; on hit proxies
to serve, on miss runs stdio standalone. (Note: stdio→HTTP proxy session
handshake is broken — see `AGENTS_proxy_session_management.md`. Direct HTTP
clients with `type: remote` work fine.)

**Tool surface:** `search` / `find` / `explore` / `get_chunk` / `status`.

**Repos config** at `~/.codesearch/repos.json`:
```json
{ "repos": { "<alias>": "<path>" }, "groups": { "<n>": ["<a>", "<b>"] } }
```

**Key files:**
- `src/mcp/mod.rs` — tool handlers, `MultiStoreContext`, `prefix_result_path`,
  `match_line_for_literal`, `literal_search`, `regex_has_anchorable_token`
- `src/serve/mod.rs` — `ServeState`, `RepoState`, file watchers
- `src/mcp/proxy.rs` — `McpProxy` (currently broken for tool calls; see note)
- `src/cli/mod.rs` — `IndexCommands`
- `src/index/mod.rs` — `add_to_index`, `remove_from_index`
- `src/db_discovery/repos.rs` — `ReposConfig`
- `src/fts/tantivy_store.rs` — BM25 index; `search_regex` is `#[cfg(test)]`,
  do not call it from production code
- `src/vectordb/store.rs` — `VectorStore`, `iter_all_chunks` (scan-path entry)
