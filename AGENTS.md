# AGENTS.md — `feature/mcp-multi-repo`

This is the single authoritative instruction file for any coding agent (OpenCode, Claude Code) working on this branch. It replaces `AGENTS_multi_repo_phase2.md`, which is now superseded.

---

## Build Rules (MANDATORY — NEVER VIOLATE)

### Target directory
- **Must be**: `C:\WorkArea\AI\codesearch\target`
- **Never**: `C:\WorkArea\AI\codesearch\codesearch.git\target`
- Controlled by `.cargo/config.toml` (`target-dir = "../target"`)

### Build type
- **Always**: DEBUG builds
- **Never**: `--release` — forbidden, causes version mismatch issues

```bash
# ✅ Correct
cd codesearch.git && cargo build
cd codesearch.git && cargo test
cd codesearch.git && cargo run -- mcp

# ❌ Forbidden
cargo build --release
cargo run --release
```

### Index rules during development
```bash
# ✅ Safe
codesearch index list

# ❌ Never — breaks running MCP sessions
codesearch index
codesearch index -f
```

---

## Code Style

### Imports
- `use crate::` for internal modules (not `use codesearch::`)
- Group: std → external crates → internal
- `use anyhow::{Result, anyhow}` for error handling
- `use tracing::{debug, info, warn}` for logging

### Error handling
- Return `anyhow::Result<T>` from fallible functions
- Never `.unwrap()` or `.expect()` in library code
- Mutex: `.lock().map_err(|e| anyhow::anyhow!("Mutex poisoned: {}", e))?`
- Use `?` for propagation, `.context()` for additional context

### Types & naming
- `PathBuf` for owned paths, `&Path` for borrowed
- `String` for owned, `&str` for borrowed (prefer `&str` in function args)
- `Arc<Mutex<T>>` for shared mutable state, `Arc` for shared read-only
- Pre-allocate: `HashMap::with_capacity(size)`

### Async
- `tokio::spawn` for background tasks
- `tokio::sync::RwLock` for async shared state
- `#[tokio::main]` for async main

### Testing
- `#[cfg(test)]` modules, `#[test]` functions
- Tests in same file as code, `use super::*;` in test module

### Serialization
- `#[derive(Serialize, Deserialize)]`
- `#[serde(skip_serializing_if = "Option::is_none")]` for optional fields

### Performance
- Streaming indexing: process files one at a time
- Embedding cache: 500MB limit via weigher-based eviction
- LMDB map_size: 2GB is sufficient
- Avoid large Vec/HashMap accumulations during processing
- Expected peak memory: ~500–700MB for large codebases

### Signal handling
- Graceful CTRL-C via `tokio::select!` + `tokio::signal`
- Exit code 130 on SIGINT
- Close all DB handles before exit

---

## What to implement on this branch

### Context

Phase 1 (already merged to this branch) built the infrastructure: `src/serve/mod.rs`, `src/mcp/proxy.rs`, `src/db_discovery/repos.rs` with groups, CLI subcommands, and the consolidated tool surface (`search`, `find`, `explore`, `get_chunk`, `status`).

**Phase 1 infrastructure that works:**
- `codesearch serve` binds HTTP + `/health` + streamable `/mcp`
- `codesearch mcp` detects running serve → proxy mode, else → stdio mode
- `McpProxy` handles lifecycle, version mismatch, dead-session errors
- `ServeState::get_or_open_stores(alias)` lazy-opens with write→readonly→Conflicted fallback
- `ReposConfig` with `repos` + `groups`, legacy migration, group management
- All tool request types accept `project: Option<String>` and `group: Option<String>`

**What still doesn't work (your task):**
- Every tool call in serve mode fails — handlers still use `self.db_path` (placeholder `"serve://multi-repo"`) instead of routing via `ServeState`
- Group queries don't fan out across repos
- Output paths don't include the alias prefix
- `project`/`group` params are silently ignored in stdio mode
- `index add` doesn't auto-register in `repos.json`; `index rm` doesn't auto-unregister
- `repos` subcommand exists but should be removed (fold into `index`)
- A running serve doesn't notice new/removed repos until restart
- Acceptance tests are missing
- README is substantially out of date

---

## Scope — what to implement

1. **Tool-handler routing** via `ServeState` when `project`/`group` provided
2. **Cross-repo fan-out + rank-based RRF merge** for group queries
3. **Alias-prefixed paths** in all output (`alias/src/file.rs:42`)
4. **Validation errors** for `project`/`group` in stdio mode
5. **CLI consolidation** — remove `repos` subcommand, fold into `index`
6. **Auto-register / auto-unregister** — symmetric: `index add` registers, `index rm` unregisters
7. **Config reload on demand** — `ServeState` detects `repos.json` mtime changes
8. **README update** — full pass to reflect all of the above
9. **Acceptance tests**

## Scope — what NOT to touch

- `src/serve/mod.rs` `run_serve()` — works as-is
- `src/mcp/proxy.rs` — leave proxy mechanics alone
- `src/db_discovery/repos.rs` `ReposConfig` — complete; may add `load_if_changed_since` helper only
- `groups` CLI subcommand — stays, operates on aliases not paths
- Existing single-repo stdio-mode behavior — must stay unchanged when no serve is running
- Consolidated tool surface (`search`/`find`/`explore`/`get_chunk`/`status`) — don't re-design
- HTTP transport, authentication, non-localhost binding — deferred

---

## Key design decisions

### RepoContext + resolve_contexts

Add to `src/mcp/mod.rs`:

```rust
pub(crate) struct RepoContext {
    pub alias: Option<String>,       // Some in serve mode, None in stdio
    pub project_path: PathBuf,
    pub db_path: PathBuf,
    pub shared_stores: Arc<SharedStores>,
    pub dimensions: usize,
    pub model_type: ModelType,
}

// Returns Vec<RepoContext> — length 1 for single-repo, N for group fan-out
fn resolve_contexts(&self, project: Option<&str>, group: Option<&str>)
    -> Result<Vec<RepoContext>, String>
```

Resolution rules:
- **stdio mode** (`shared_stores` is Some, `serve_state` is None):
  - `project` or `group` provided → `Err("project/group routing requires codesearch serve")`
  - both None → `Ok(vec![ctx_from_self])`
- **serve mode** (`serve_state` is Some):
  - `group` → `resolve_group(group)` → one context per alias
  - `project` → `resolve(project)` → one context
  - both None → `Err("this serve instance requires project or group parameter")`
  - both Some → `Err("pass either project or group, not both")`
- **proxy mode** (`proxy` is Some): never reached — forwarded before this point

Move `with_vector_store_read` / `with_fts_store_read` to `impl RepoContext`.

### Proxy forwarding

Every handler starts with this guard (pattern already in `list_projects`):

```rust
if let Some(ref proxy) = self.proxy {
    let params = serde_json::to_value(&request).unwrap_or(serde_json::Value::Null);
    return proxy.forward("tool_name", Some(params)).await
        .map_err(|e| McpError::internal_error(e.message.into_owned(), None));
}
```

Apply consistently to every tool handler. Delete all `let _project = ...` no-ops.

### Cross-repo fan-out (group queries)

Applies to: `semantic_search`, `literal_search`, `find_definition`, `find_usages`, `find_dependents`.

Single-repo tools (`file_outline`, `get_chunk`, `similar_chunks`, `find_imports`) with `group` → error: *"Tool X operates on a single repo. Use `project` instead of `group`."*

Fan-out pattern:
```rust
// 1. Per-repo in parallel (per_repo_limit = limit * 3)
// 2. Prefix all paths: format!("{}/{}", alias, relative_path)
// 3. Rank-based RRF merge — score = 1/(k+rank), k=60
//    Dedup key: (alias, chunk_id) — chunk_ids not globally unique
```

Path prefix helper:
```rust
fn prefix_path_with_alias(path: &str, alias: Option<&str>, project_root: &str) -> String {
    let relative = path.strip_prefix(project_root).unwrap_or(path).trim_start_matches('/');
    match alias {
        Some(a) => format!("{}/{}", a, relative),
        None => path.to_string(),
    }
}
```

Apply prefix in serve mode even for single-project calls.

### CLI consolidation — `repos` subcommand removed

Remove `ReposCommands` enum and the `Repos { command: ReposCommands }` variant from `Commands`. No deprecated alias — pre-1.0, never shipped.

**Final CLI surface for repo management:**

```
codesearch index add <path> [--alias NAME] [--global]
codesearch index rm  <path> [--keep-config]
codesearch index list
codesearch groups add <name> --aliases <alias>...
codesearch groups rm  <name>
codesearch groups list
```

**`index add` behavior:**
- Canonicalize path
- Create `.codesearch.db/` if it doesn't exist (skip if already exists, print `ℹ️ Index already exists, reusing.`)
- Load `ReposConfig`. If path already registered: print `ℹ️ Already registered as '{alias}'.` and stop — no re-register
- Otherwise: `register_with_alias(path, --alias)`. Alias collision with different path → error
- `--global`: skip auto-register, print `⚠️ Global indexes are not auto-registered. Use index add without --global if you want serve to discover it.`
- Success: `✅ Indexed {path} as '{alias}'.`
- Print failure: warn but do NOT fail the index operation itself if `repos.json` write fails

**`index rm` behavior:**
- Canonicalize path
- Look up alias via `alias_for_path`. If none: no config write (wasn't registered), proceed with DB deletion only
- Unless `--keep-config`: call `unregister_path(&path)` → removes alias from `repos.json` and from any groups containing it; drops empty groups. Save.
- Delete `.codesearch.db/` directory. Windows "file in use" error → warn: `⚠️ Database may be locked. Stop codesearch serve and retry.`
- `--keep-config`: delete DB, leave config entry, print `ℹ️ Config entry preserved.`
- If DB deletion fails: config entry stays (safe, reversible). If config write fails: DB is deleted, warn to clean up `repos.json` manually.

**`index list` behavior:**
- Read `ReposConfig`
- For each alias: alias, path, DB exists, chunks, files, model, lock_status (best-effort from serve HTTP if reachable)
- Tabular output + `--json`
- Show groups at bottom: `group → [alias1, alias2, ...]`
- Bottom hint: `Current directory: {path} → {alias or 'not registered'}`

**Worktree note:** Each worktree is a distinct project root (already handled by `find_git_root`). `unique_alias_for_path` auto-generates distinct aliases (`codesearch`, `codesearch-2`). Correct by default.

### Config reload on demand in serve

Add to `ServeState`:

```rust
config: RwLock<ReposConfig>,
config_mtime: RwLock<Option<SystemTime>>,
```

Private helper `reload_if_changed(&self) -> Result<()>`:

1. `stat` the config path. Equal mtime → no-op.
2. On change: `ReposConfig::load()`. Parse error → log, keep old, update mtime (avoid retry storm).
3. Compute `old.repos.keys() - new.repos.keys()` = removed aliases.
4. For each removed alias: `self.repos.remove(alias)`. The `Arc<SharedStores>` drop chain closes LMDB + stops FSW.
5. Replace `self.config`. Update `self.config_mtime`.
6. Do NOT pre-open new aliases — lazy-open on first query.

Call `reload_if_changed` at the top of: `get_or_open_stores`, `aliases()`, `resolve_alias`.

Use `RwLock<ReposConfig>` (reads frequent, writes rare). Collect "to remove" list under write lock, release lock, then do `self.repos.remove()` calls — don't hold write lock while dropping stores.

**Race safety:** In-flight calls hold their own `Arc<SharedStores>` clone — they finish cleanly even if the alias is reloaded away. Refcount hits zero after the call drops → LMDB closes.

No file watcher on `repos.json` — mtime-check is sufficient.

**`serve --register` interaction:** The flag already saves config before `ServeState::new`, so initial mtime is correct. No spurious reload on first query.

---

## File-by-file plan

### `src/mcp/mod.rs`

1. Define `RepoContext` near the top.
2. Add `resolve_contexts` on `CodesearchService`.
3. Move `with_vector_store_read` / `with_fts_store_read` to `impl RepoContext`.
4. Update every tool handler — full list:
   - `semantic_search`, `literal_search`, `find_definition`, `find_usages`, `find_usages_impl`, `find_references`, `find_imports`, `find_dependents`, `similar_chunks`, `file_outline`, `get_chunk`, `index_status`, `list_projects`, `find_databases`
5. For each handler: proxy-forward guard first, then `resolve_contexts`, then adapt logic.
6. Delete all `let _project = request.project.as_deref();` no-ops.
7. Add `rrf_merge_by_rank` pure helper function.
8. Add `prefix_path_with_alias` pure helper function.
9. Extend `list_projects` serve-mode path to use live `serve_state` lock status.

### `src/cli/mod.rs`

1. Remove `ReposCommands` enum.
2. Remove `Repos { command: ReposCommands }` from `Commands`.
3. Add `--alias: Option<String>` flag to `IndexCommands::Add`.
4. Add `--keep-config: bool` flag to `IndexCommands::Remove`.
5. Rewrite `IndexCommands::List` handler to read `ReposConfig` (all registered repos + groups).

### `src/serve/mod.rs`

1. Add `config: RwLock<ReposConfig>` and `config_mtime: RwLock<Option<SystemTime>>` to `ServeState`.
2. Implement `reload_if_changed`.
3. Call it at the top of `get_or_open_stores`, `aliases()`, `resolve_alias`.

### CLI handler dispatch (wherever `Commands::Index { … }` is matched)

1. `Add` arm: run indexer + `register_with_alias` + save.
2. `Remove` arm: `unregister_path` (unless `--keep-config`) + delete DB directory.
3. `List` arm: read `ReposConfig`, format table.
4. Remove `Repos` arm entirely.

### `README.md`

See §README update below.

---

## Acceptance criteria

All must pass before PR merge:

- `cargo test --all` passes
- `cargo clippy --all-targets -- -D warnings` passes

**Routing:**
- `resolve_contexts_stdio_rejects_project`
- `resolve_contexts_serve_rejects_missing_project`
- `resolve_contexts_serve_rejects_both`
- `resolve_contexts_serve_group_fans_out`: group with 2 repos → 2 contexts

**Merge + paths:**
- `rrf_merge_by_rank_pure`: deterministic unit test
- `path_prefix_with_alias`: unit test for prefix behavior

**Serve + proxy:**
- `health_endpoint_live`: spawn serve on ephemeral port, GET `/health`, assert JSON shape + version
- `version_mismatch_errors`: mock different version → `check_health` returns `Err`
- `stdio_fallback_when_no_serve`: `McpProxy::check_health(nonexistent_url) → Ok(false)`
- `lock_invariant_windows`: two processes, one DB, assert exactly one write-opens — `#[cfg(target_os = "windows")]`
- `conflicted_repo_isolated`: stdio holds write-lock on X; serve starts; X → Conflicted; other repos still work

**Cross-repo:**
- `cross_repo_search_group`: 2 indexed repos, `group="both"` → results from both, alias-prefixed, no duplicate `(alias, chunk_id)`
- `single_repo_tools_reject_group`: `file_outline`, `get_chunk`, `similar_chunks`, `find_imports` with `group=Some(...)` → error

**CLI consolidation:**
- `cli_no_repos_subcommand`: `codesearch repos --help` returns CLI error
- `index_add_auto_registers`: temp dir, `index add` → entry in `repos.json`; second call → `Already registered`, no duplicate
- `index_add_with_explicit_alias`: `index add /tmp/x --alias myrepo` → alias `myrepo` in `repos.json`
- `index_add_alias_collision`: existing alias `foo` → `index add /tmp/b --alias foo` → error, no write
- `index_add_global_skips_register`: `--global` → `repos.json` unchanged, warning emitted
- `index_rm_auto_unregisters`: registered repo in group → `index rm` → alias gone from `repos.json` and from group; empty group dropped
- `index_rm_preserves_config_with_flag`: `--keep-config` → DB deleted, config entry remains
- `index_rm_unregistered_path_ok`: unregistered path → no error, DB deleted
- `index_list_shows_registered_repos`: 2 repos + 1 group → all appear

**Config reload:**
- `config_reload_picks_up_new_alias`: add repo B to `repos.json` after ServeState init → `aliases()` includes B, `get_or_open_stores("B")` succeeds
- `config_reload_drops_removed_alias`: remove repo B from `repos.json` → `get_or_open_stores("B")` errors, entry removed from DashMap
- `config_reload_no_spurious_reload`: two calls without touching `repos.json` → `ReposConfig::load` called at most once

All existing tests must still pass, including `test_mcp_no_raw_stdout_calls`.

---

## Implementation order

1. `RepoContext` + `resolve_contexts` — pure helpers, unit-test first
2. Single-repo routing end-to-end on `index_status` — smoke-test manually: `serve --register . && mcp && status`
3. All remaining single-repo handlers
4. `rrf_merge_by_rank` + fan-out logic
5. Group-capable handlers
6. Single-repo-tool rejection for `group`
7. `prefix_path_with_alias` + apply everywhere
8. `list_projects` serve-mode live lock status
9. CLI consolidation: remove `ReposCommands`, add `--alias`/`--keep-config`, implement auto-register, auto-unregister, rewrite list
10. `reload_if_changed` in `ServeState`
11. README update
12. Acceptance tests

One commit per step.

---

## README update (required as part of this branch)

The README is substantially out of date. These sections need a full rewrite:

**Remove / replace:**
- `codesearch repos add/rm/list` from "Other Commands" table and "Repository Management" section — replaced by `index add/rm/list`
- Old individual tool names as primary tools — new primary surface is `search`/`find`/`explore`/`get_chunk`/`status`

**Rewrite "Repository Management" → "Index & Project Management":**
- `codesearch index add <path> [--alias NAME]` — create index AND register
- `codesearch index rm <path>` — remove index AND unregister; symmetric with add
- `codesearch index list` — show all registered repos + index state + groups
- Note: re-indexing (`codesearch index` without `add`) leaves config alone

**Add "Groups" section with use-case explanation:**
> Groups let you search across multiple related repos in a single call — useful for refactoring across a shared library and its consumers, or for finding where a symbol is used across a whole platform.
>
> Groups are created manually — you know which repos belong together; an AI agent doesn't.
```bash
codesearch groups add platform --aliases shared-lib service-a service-b
codesearch groups list
codesearch groups rm platform
```
> In an AI session: `search(mode="semantic", group="platform", query="where is X used?")` fans out across all repos in the group and returns merged, alias-prefixed results.

**Update "MCP Serve Mode" section:**
- Explain proxy auto-detect clearly: stdio MCP probes `/health` at startup, enters proxy mode if serve is running, falls through to local mode if not
- Version mismatch → hard error (not silent)
- Dead-session behavior (serve dies mid-session → all subsequent calls error, no silent fallback)

**Update "MCP Tools" table:**
- Show `search(mode="semantic"|"literal")`, `find(kind=...)`, `explore(kind=...)`, `get_chunk`, `status(kind=...)` as primary
- Old tool names listed only as "deprecated aliases" — same section structure as it is now but shorter

**Update "Other Commands" table:**
Remove `repos` rows. Replace with:
```
codesearch index add [PATH] [--alias NAME]   Create index and register
codesearch index rm  [PATH] [--keep-config]  Remove index and unregister
codesearch index list                          Show all registered repos + groups
codesearch groups add <n> --aliases A B...   Create/update a group
codesearch groups rm  <n>                    Remove a group
codesearch groups list                         List all groups
```

**Troubleshooting table:** add row for "Project not visible to serve after index add → restart not needed; serve detects config changes automatically on next query".

---

## Out of scope

- Remote serve (non-localhost)
- HTTP authentication / OAuth
- Auto-start of serve from mcp (forking) — explicitly rejected
- Indexer-side dedup of stale chunks — separate future branch
- Non-AST file indexing (Markdown, YAML, configs)
- Import-graph persistence
- Query expansion
- File-watcher-based config reload — mtime-check is sufficient
- `index rename` command — edit `repos.json` or `rm` + `add --alias`
