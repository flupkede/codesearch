# AGENTS.md — `feature/mcp-multi-repo`

Branch is feature-complete. Only PR hygiene remains. Keep this file short — it exists
to orient a new agent coming into the branch, not to narrate history.

---

## Status

All planned implementation work on this branch is done and verified:

- `cargo check --lib` — clean
- `cargo test --lib` — 304 passed / 0 failed (12 ignored, integration-only)
- `cargo clippy --all-targets -- -D warnings` — clean

**Uncommitted changes in the working tree** (all green):

- `src/mcp/mod.rs` — multi-repo path prefixing, alias-aware dedup,
  `MultiStoreContext::prefix_result_path` method (with debug logging for
  unresolved-alias cases), refreshed `search` tool description
- `AGENTS.md` — this file
- `AGENTS_auto-regex_and_confidence.md` — scoped plan for the **next** branch,
  not part of this PR

---

## TODO — in order

### 1. Full test suite
```powershell
cargo test --all
```
Runs integration tests that `--lib` skips. Must be green before commit.

### 2. Spot-check the diff
```powershell
git diff --stat
git diff src/mcp/mod.rs | Select-Object -First 200
```
You're looking for:
- No leftover `prefix_path_for_ctx` free-function (it was refactored into
  `MultiStoreContext::prefix_result_path`)
- No leftover `let project_alias = ctx.project_alias.clone()` patterns (they were
  removed when the method replaced the free function)
- All `ctx.stores` / `ctx.stores_vec` passed into `with_*_store_read_for*` now use
  `.clone()` (needed because the method consumes by value; Arc::clone is cheap)
- The 4 prefix tests exist and are in the `tests` module:
  `test_group_results_are_alias_prefixed`,
  `test_single_project_result_is_alias_prefixed`,
  `test_dedup_key_includes_alias`,
  `test_stdio_mode_paths_not_prefixed`

### 3. Stage and commit
Single commit, suggested message:
```
feat(mcp): multi-repo path prefixing with alias-aware dedup

- Apply prefix_path_with_alias across all fan-out handlers via new
  MultiStoreContext::prefix_result_path method (3-mode dispatch:
  single-project / group / stdio) with debug logging for unresolved aliases
- Switch cross-store dedup key from chunk_id to (alias, chunk_id)
- Refresh search tool description with explicit regex/phrase sub-mode
  guidance and examples to reduce external-grep fallback
- Add 4 tests for prefix + dedup behaviour
```

### 4. Push and open PR
```powershell
git push origin feature/mcp-multi-repo
```
PR description must note: `AGENTS_auto-regex_and_confidence.md` is a follow-up plan
for a separate branch, not part of this PR's scope. Reviewers should not treat it as
pending work against this branch.

---

## Build Rules (keep this short, it's reference material)

- Target directory: `C:\WorkArea\AI\codesearch\target` (set by `.cargo/config.toml`)
- **Always DEBUG builds. `--release` is forbidden** (indexing releases produce
  incompatible artifacts for this branch's serve/proxy model).
- **Only use MCP tools for editing**. Bash/PowerShell only for cargo commands.
- Never run `codesearch index` from inside this repo — it breaks running MCP
  sessions that are attached to the same DB.

---

## Code Style (keep this short, it's reference material)

- `anyhow::Result<T>` for fallible functions; never `.unwrap()`/`.expect()` in
  library code
- Windows path hygiene: normalize via `crate::cache::normalize_path_str` before
  comparing, prefixing, or stripping paths
- No `print!` / `println!` / `eprintln!` in `src/mcp/` — enforced by
  `test_mcp_no_raw_stdout_calls`
- Deterministic tests only. No `sleep`. Use `tokio::sync::Barrier` or explicit
  signals when synchronisation is needed.

---

## Project Architecture (needed orientation, keep concise)

`codesearch serve` binds `127.0.0.1:39725` (env `CODESEARCH_SERVE_PORT`), exposes
`GET /health` and MCP streamable HTTP at `/mcp`. `codesearch mcp` probes `/health`
at startup (200 ms timeout): match → proxy mode; miss → stdio mode; version mismatch
→ hard error with three-step fix.

**Repos config** `~/.codesearch/repos.json`:
```json
{ "repos": { "<alias>": "<path>" }, "groups": { "<name>": ["<alias1>", "<alias2>"] } }
```

**Tool surface**: `search` / `find` / `explore` / `get_chunk` / `status`.

**Key constants**: `DEFAULT_SERVE_PORT=39725`, `SERVE_PORT_ENV="CODESEARCH_SERVE_PORT"`,
`HEALTH_PATH="/health"`, `MCP_ENDPOINT_PATH="/mcp"`, `HEALTH_PROBE_TIMEOUT_MS=200`.

**Key files**:
- `src/mcp/mod.rs` — tool handlers, `MultiStoreContext`, `prefix_result_path`
- `src/serve/mod.rs` — `ServeState`, three-variant `RepoState`, file watchers
- `src/mcp/proxy.rs` — `McpProxy` with version-mismatch error
- `src/cli/mod.rs` — `IndexCommands` (Add/Remove with alias/keep_config flags)
- `src/index/mod.rs` — `add_to_index`, `remove_from_index`
- `src/db_discovery/repos.rs` — `ReposConfig`
