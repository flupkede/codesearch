# AGENTS.md — `feature/mcp-multi-repo`

Single authoritative instruction file for OpenCode / Claude Code on this branch.

---

## Build Rules (MANDATORY)

- Target directory: `C:\WorkArea\AI\codesearch\target` (set by `.cargo/config.toml`)
- **Always DEBUG builds. `--release` is forbidden.**
- **ONLY USE MCP TOOLS** for code exploration and editing. Bash only for `cargo build`, `cargo test`, `cargo clippy`.

```bash
cd codesearch.git && cargo build    # ✅ correct
cd codesearch.git && cargo test     # ✅ correct
cargo build --release               # ❌ forbidden
codesearch index                    # ❌ never — breaks running MCP sessions
```

---

## Code Style

- `use crate::` for internal imports. Group: std → external → internal.
- `anyhow::Result<T>`, never `.unwrap()`/`.expect()` in library code.
- **Windows path hygiene**: always normalize through `crate::cache::normalize_path_str` before comparing, prefixing, or stripping paths.
- No `print!`/`println!` in `src/mcp/` — enforced by `test_mcp_no_raw_stdout_calls`.
- Deterministic tests only — `tokio::sync::Barrier` or explicit signals, not `sleep`.

---

## Project Architecture

**Serve/proxy mode.** `codesearch serve` binds `127.0.0.1:39725` (env `CODESEARCH_SERVE_PORT`), exposes `GET /health` and MCP streamable HTTP at `/mcp`. `codesearch mcp` probes `/health` at startup (200ms timeout): match → proxy mode; miss → stdio mode; version mismatch → hard error.

**Repos config** `~/.codesearch/repos.json`:
```json
{ "repos": { "<alias>": "<path>" }, "groups": { "<name>": ["<alias1>", "<alias2>"] } }
```

**Tool surface**: 5 primary tools — `search` / `find` / `explore` / `get_chunk` / `status`.

**Key constants**: `DEFAULT_SERVE_PORT=39725`, `SERVE_PORT_ENV="CODESEARCH_SERVE_PORT"`, `HEALTH_PATH="/health"`, `MCP_ENDPOINT_PATH="/mcp"`, `HEALTH_PROBE_TIMEOUT_MS=200`.

**Key files**: `src/mcp/mod.rs` (tool handlers), `src/serve/mod.rs` (ServeState, RepoState, file watchers), `src/mcp/proxy.rs` (McpProxy), `src/cli/mod.rs` (CLI), `src/index/mod.rs` (add_to_index, remove_from_index), `src/db_discovery/repos.rs` (ReposConfig).

---

## What's done — do NOT touch

Everything below compiles, tests pass, leave alone:

- `MultiStoreContext` + `resolve_routing()` — handlers route via `serve_state.get_or_open_stores(alias)` and `resolve_group_aliases`
- `validate_project_group` helper + 8 tests (`src/mcp/types.rs`)
- `HasChunkId`/`HasScore` traits + cross-store dedup + 20 tests
- `RepoState` three-variant: `Write { stores, index_manager: Option<Arc<IndexManager>>, cancel_token }` / `Readonly { stores }` / `Conflicted`
- `ServeState` with `RwLock<ReposConfig>` + `config_mtime` + `reload_if_changed` + `config_path_override` for tests
- `get_or_open_stores`: db-exists precheck (not cached as Conflicted), `conflicted_msg`, per-repo IndexManager+FSW spawned on write-open, cancel_token fired on reload-removal
- `repo_lock_status(&str)` and `config_snapshot()` helpers on `ServeState`
- `list_projects` uses `serve_state.repo_lock_status` in serve mode, disk-check fallback for unopened aliases
- CLI: `ReposCommands` removed, `IndexCommands::Add` has `alias: Option<String>`, `IndexCommands::Remove` has `keep_config: bool`, error text updated to `'codesearch index add'`
- `add_to_index` auto-registers via `register_with_alias` + `config.save()` (skips `--global`)
- `remove_from_index` auto-unregisters via `unregister_path` (respects `--keep-config`, Windows lock warning)
- `proxy.rs`: version-mismatch error includes three-step fix with `(1) stop (2) install (3) restart`
- 5 `#[tool]` attributes only (deprecated aliases removed), instructions trimmed to ≤ 50 lines
- README: repos commands removed, deprecated aliases table removed, Groups section added, file-watcher + config-reload docs added
- `prefix_path_with_alias` helper written + 6 tests in `src/mcp/mod.rs`

**Tests currently passing: ~300** (exact count from last local build).

---

## Remaining work — 1 item

### Apply `prefix_path_with_alias` in all group fan-out handlers

**Status.** The helper `prefix_path_with_alias` exists at `src/mcp/mod.rs:1792` and has 6 passing tests, but carries `#[allow(dead_code)]` because it is **never called**. Result paths in group queries are bare absolute paths — the LLM cannot tell which repo a result comes from when two repos share the same relative path (e.g. both have `src/main.rs`).

**What to do.** Remove `#[allow(dead_code)]` and apply the helper inside every handler that performs a group fan-out loop. The pattern is: after collecting results for one alias, rewrite each `.path` field before merging into `all_results`.

```rust
// Inside the per-alias loop for any multi-store handler:
for alias in &aliases {
    let stores = serve_state.get_or_open_stores(alias).await?;
    let project_path = {
        let cfg = serve_state.config_snapshot();
        cfg.resolve(alias)
            .map(|p| crate::cache::normalize_path_str(p.to_string_lossy().as_ref()))
            .unwrap_or_default()
    };
    let mut items = run_search_on_stores(&stores, ...).await?;
    for item in &mut items {
        item.path = prefix_path_with_alias(&item.path, Some(alias), &project_path);
    }
    all_items.extend(items);
}
```

**Apply to every handler that fan-outs over a group.** Search `src/mcp/mod.rs` for the pattern `for alias in` or `stores_vec` or `with_fts_store_read_multi` to find all call sites. The result types that have a `.path` field and need prefixing are:
- `SearchResultItem` (in `search(mode="semantic")` and `search(mode="literal")`)
- `ReferenceItem` (in `find(kind="usages")` and `find(kind="dependents")`)
- `FileOutlineItem` (in `explore(kind="outline")` — single-repo only, reject `group` with clear error: `"Tool 'explore' operates on a single repo. Use 'project' instead of 'group'."`)
- `ImportItem` / `DependentItem` paths (in `find(kind="imports")` and `find(kind="dependents")`)

For **single-project** calls in serve mode (project= given, not group=), also apply the prefix — the LLM still needs to know which repo the result belongs to when working with multiple registered repos.

For **stdio mode** (`serve_state` is `None`), `alias` is `None` → `prefix_path_with_alias` returns the normalized path only (no prefix). No change in behavior for single-repo users.

**Companion fix: dedup key.** The existing dedup logic in `with_fts_store_read_multi` and `with_vector_store_read_multi` deduplicates by `chunk_id` alone. After prefixing, `chunk_id` values are still per-repo integers that can collide across repos (both repo-a and repo-b may have a chunk with id=42 meaning different things). The dedup key must be `(alias, chunk_id)` — not just `chunk_id`. Fix the dedup maps accordingly:

```rust
// Before (wrong for multi-repo):
let mut seen_ids: HashMap<u32, usize> = HashMap::new();

// After (correct):
let mut seen_ids: HashMap<(String, u32), usize> = HashMap::new();
// key = (alias.to_string(), HasChunkId::chunk_id(r))
```

**Tests to add** (in `src/mcp/mod.rs` test module):

- `test_group_results_are_alias_prefixed`: simulate two stores for aliases `"a"` and `"b"`, each returning a result with `path = "src/main.rs"`. After applying `prefix_path_with_alias`, assert results have `path = "a/src/main.rs"` and `b/src/main.rs"` respectively.
- `test_single_project_result_is_alias_prefixed`: single store for alias `"myrepo"`, result with `path = "/abs/root/src/lib.rs"`, project root `"/abs/root"` → assert path becomes `"myrepo/src/lib.rs"`.
- `test_dedup_key_includes_alias`: two stores each returning `chunk_id=1`, different content. Assert both are kept after merge (key = `(alias, chunk_id)`, not just `chunk_id`).
- `test_stdio_mode_paths_not_prefixed`: alias `None` → path normalized, no prefix added.

---

## Acceptance criteria

- `cargo test --all` passes
- `cargo clippy --all-targets -- -D warnings` passes (no `#[allow(dead_code)]` on `prefix_path_with_alias`)
- `test_mcp_no_raw_stdout_calls` passes
- `test_instructions_max_50_lines` passes
- Group query smoke test: `search(group="pair", query="fn")` returns results with paths like `"repo-a/src/main.rs"` and `"bravo/src/main.rs"`, never bare paths
