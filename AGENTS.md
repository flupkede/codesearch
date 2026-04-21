# AGENTS.md — `feature/mcp-multi-repo`

This is the single authoritative instruction file for any coding agent (OpenCode, Claude Code) working on this branch.

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
- **Always make errors actionable.** The error message must tell the user what to do next — which command to run, what setting to change, or what to check. "X failed" without a fix is a bug.

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
- For race conditions: use explicit `tokio::sync::Barrier` or serialized events rather than `tokio::time::sleep` — deterministic tests only

### Serialization
- `#[derive(Serialize, Deserialize)]`
- `#[serde(skip_serializing_if = "Option::is_none")]` for optional fields

### Path normalization (CRITICAL on Windows)
- Always normalize through `crate::cache::normalize_path_str` before comparing, prefixing, or stripping paths
- Never construct paths with raw `format!` when the input could be a Windows path — backslashes leak through and produce mixed separators

### Performance
- Streaming indexing: process files one at a time
- Embedding cache: 500MB limit via weigher-based eviction
- LMDB map_size: 2GB is sufficient
- Expected peak memory: ~500–700MB for large codebases

### Signal handling
- Graceful CTRL-C via `tokio::select!` + `tokio::signal`
- Exit code 130 on SIGINT
- Close all DB handles before exit

---

## Context

Phase 1 (already merged to this branch) built the infrastructure: `src/serve/mod.rs`, `src/mcp/proxy.rs`, `src/db_discovery/repos.rs` with groups, CLI subcommands, and the consolidated tool surface (`search`, `find`, `explore`, `get_chunk`, `status`).

**Phase 1 that works:**
- `codesearch serve` binds HTTP + `/health` + streamable `/mcp`
- `codesearch mcp` detects running serve → proxy mode, else → stdio mode
- `McpProxy` handles lifecycle, version mismatch, dead-session errors
- `ServeState::get_or_open_stores(alias)` lazy-opens with write→readonly→Conflicted fallback
- `ReposConfig` with `repos` + `groups`
- All tool request types accept `project: Option<String>` and `group: Option<String>`

**What still doesn't work (your task):**
- Every tool call in serve mode fails — handlers still use `self.db_path` placeholder instead of routing via `ServeState`
- Group queries don't fan out across repos
- Output paths don't include the alias prefix
- `project`/`group` params silently ignored in stdio mode
- `index add` doesn't auto-register; `index rm` doesn't auto-unregister
- `repos` subcommand exists but should be removed (fold into `index`)
- A running serve doesn't notice new/removed repos until restart
- **Serve has no file watcher per repo — file changes aren't picked up until manual re-index**
- Several correctness issues (error discrimination, path normalization on Windows, error actionability)
- `src/server/` (old REST API from the fork, orphaned dead code)
- Deprecated tool aliases inflate the LLM-visible tool list to 17 — should be trimmed
- Acceptance tests missing
- README substantially out of date

---

## Scope

1. **Tool-handler routing** via `ServeState` when `project`/`group` provided
2. **Cross-repo fan-out + rank-based RRF merge** for group queries
3. **Alias-prefixed paths** in all output (Windows-safe)
4. **Validation errors** for `project`/`group` in stdio mode
5. **CLI consolidation** — remove `repos` subcommand, fold into `index`
6. **Auto-register / auto-unregister** symmetry in `index add` / `index rm`
7. **Config reload on demand** — `ServeState` detects `repos.json` mtime changes
8. **Per-repo file watchers in serve** — IndexManager + FSW + git HEAD watcher per writable repo
9. **Correctness fixes** — db-exists precheck, Windows path normalization, actionable error messages
10. **Cleanup** — remove orphaned `src/server/` module, remove deprecated MCP tool aliases
11. **README update** — full pass reflecting all of the above
12. **Acceptance tests**

## Scope — what NOT to touch

- `src/mcp/proxy.rs` — proxy mechanics are correct; leave them
- `src/db_discovery/repos.rs` `ReposConfig` public API — complete (optional internal helpers allowed)
- `groups` CLI subcommand — stays, operates on aliases
- Existing single-repo stdio-mode behavior — must stay unchanged when no serve is running
- Consolidated tool surface names (`search`/`find`/`explore`/`get_chunk`/`status`) — don't rename
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
    pub readonly: bool,              // true when store was opened readonly (no writer)
}

fn resolve_contexts(&self, project: Option<&str>, group: Option<&str>)
    -> Result<Vec<RepoContext>, String>
```

Resolution rules:
- **stdio mode** (`shared_stores` Some, `serve_state` None):
  - `project`/`group` provided → `Err("project/group routing requires codesearch serve to be running. Start serve and reconnect the MCP client.")`
  - both None → `Ok(vec![ctx_from_self])`
- **serve mode** (`serve_state` Some):
  - `group` → `resolve_group(group)` → one context per alias
  - `project` → `resolve(project)` → one context
  - both None → `Err("This serve instance manages multiple repos. Pass `project=<alias>` for a single repo or `group=<n>` for a group.")`
  - both Some → `Err("Pass either `project` or `group`, not both.")`
- **proxy mode** (`proxy` Some): never reached — forwarded before this point

Move `with_vector_store_read` / `with_fts_store_read` to `impl RepoContext`.

### Proxy forwarding

Every handler starts with:

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

Single-repo tools (`file_outline`, `get_chunk`, `similar_chunks`, `find_imports`) with `group` → error: *"Tool 'X' operates on a single repo. Use `project` instead of `group`."*

Fan-out pattern:
```rust
// 1. Per-repo in parallel (per_repo_limit = limit * 3)
// 2. Prefix all paths via prefix_path_with_alias
// 3. Rank-based RRF merge — score = 1/(k+rank), k=60
//    Dedup key: (alias, chunk_id) — chunk_ids not globally unique
```

### Windows-safe path prefix helper

```rust
fn prefix_path_with_alias(path: &str, alias: Option<&str>, project_root: &str) -> String {
    // Normalize both to forward slashes via the existing cache helper to
    // avoid mixed separators on Windows (e.g. "myalias/src\main.rs")
    let normalized = crate::cache::normalize_path_str(path);
    let normalized_root = crate::cache::normalize_path_str(project_root)
        .trim_end_matches('/')
        .to_string();
    let relative = normalized
        .strip_prefix(&normalized_root)
        .unwrap_or(&normalized)
        .trim_start_matches('/');
    match alias {
        Some(a) => format!("{}/{}", a, relative),
        None => normalized.to_string(),
    }
}
```

Apply in serve mode even for single-project calls. Unit test must cover mixed Windows/Unix input.

### CLI consolidation — `repos` subcommand removed

Remove `ReposCommands` enum and the `Repos` variant from `Commands`. No deprecated alias — pre-1.0, never shipped.

**Final CLI for repo management:**
```
codesearch index add <path> [--alias NAME] [--global]
codesearch index rm  <path> [--keep-config]
codesearch index list
codesearch groups add <n> --aliases <alias>...
codesearch groups rm  <n>
codesearch groups list
```

**`index add` behavior:**
- Canonicalize path
- Skip DB creation if `.codesearch.db/` exists: `ℹ️ Index already exists, reusing.`
- Load `ReposConfig`. If path already registered → `ℹ️ Already registered as '{alias}'.`, stop
- Otherwise `register_with_alias(path, --alias)`. Alias collision with different path → error
- `--global`: skip register, warn: `⚠️ Global indexes are not auto-registered. Use index add without --global for serve discovery.`
- Success: `✅ Indexed {path} as '{alias}'.`
- Failure to write `repos.json`: warn, but do NOT fail the index op

**`index rm` behavior:**
- Canonicalize path, look up alias. Not registered → skip config write, proceed with DB deletion
- Unless `--keep-config`: `unregister_path(&path)` → drops alias from groups too, drops empty groups, save
- Delete `.codesearch.db/`. Windows "file in use" → warn: `⚠️ Database files may be locked by a running codesearch serve. Stop it and retry.`
- `--keep-config`: delete DB only, `ℹ️ Config entry preserved.`
- DB deletion fails: config stays (reversible). Config write fails but DB deleted: warn user to clean `repos.json` manually

**`index list` behavior:**
- Read `ReposConfig`. Per alias: path, DB exists, chunks, files, model, lock_status (best-effort from local serve HTTP)
- Tabular + `--json`
- Groups at bottom: `group → [alias1, alias2, ...]`
- Footer: `Current directory: {path} → {alias or 'not registered'}`

**Worktree note:** Each worktree is a distinct project root (handled by `find_git_root`). `unique_alias_for_path` auto-generates distinct aliases per worktree. Correct by default.

### Config reload on demand in serve

Add to `ServeState`:
```rust
config: RwLock<ReposConfig>,
config_mtime: RwLock<Option<SystemTime>>,
```

Private helper `reload_if_changed(&self) -> Result<()>`:
1. `stat` config path. Equal mtime → no-op.
2. On change: `ReposConfig::load()`. Parse error → log, keep old, update mtime (avoid retry storm).
3. Compute removed aliases = `old.repos.keys() - new.repos.keys()`.
4. For each removed alias: fire its cancel_token (see §Per-repo watchers), then `self.repos.remove(alias)`. Drop chain closes LMDB + stops FSW.
5. Replace `self.config`. Update `self.config_mtime`.
6. Do NOT pre-open new aliases — lazy-open on first query.

Call `reload_if_changed` at the top of: `get_or_open_stores`, `aliases()`, `resolve_alias`.

Collect "to remove" list under write lock, release, then remove from DashMap — don't hold write lock while dropping stores.

### Per-repo file watchers in serve (Option A)

**The problem.** Without this, serve is functionally broken for active development: users edit files, searches return stale results until they manually re-index (which conflicts with serve's write-lock). Stdio-mode MCP has this already via `IndexManager`; serve mode must match.

**Design.** Extend `RepoState`:

```rust
pub(crate) enum RepoState {
    /// Writable repo — full file watching + git HEAD watching active.
    Write {
        stores: Arc<SharedStores>,
        index_manager: Arc<IndexManager>,
        cancel_token: CancellationToken,
    },
    /// Another process holds the write lock. Read-only access, no live updates.
    /// Results may be stale.
    Readonly {
        stores: Arc<SharedStores>,
    },
    /// Write-lock contended AND readonly open failed. No access possible.
    Conflicted,
}
```

`get_or_open_stores` after successful **write** open:
1. Create `IndexManager::new_without_refresh(&project_path, shared_stores.clone()).await?`
2. Create a per-repo `CancellationToken`
3. `tokio::spawn` a background task that:
   - Starts FSW via `index_manager.start_watching().await`
   - Runs initial incremental refresh via `IndexManager::perform_incremental_refresh_with_stores(&project_path, &db_path, &stores)`
   - After refresh, starts the file watcher via `index_manager.start_file_watcher(cancel_token.clone()).await`
   - On cancel_token trigger: drops cleanly
4. Store `RepoState::Write { stores, index_manager, cancel_token }` in DashMap

`get_or_open_stores` after successful **readonly** open → `RepoState::Readonly { stores }`. No watcher. Tool handlers must surface the readonly status to the LLM in `list_projects` output.

**Teardown.** When `reload_if_changed` removes an alias:
- If state is `Write { cancel_token, .. }`: fire the token. The spawned task sees it and exits.
- Remove from DashMap. Arc-drop closes LMDB.

**Concurrency note.** Each `IndexManager` owns its own `RwLock`-guarded writes to `SharedStores`. Tool calls hold reader guards; the watcher holds writer guards during refresh. No coordination across repos is needed — each is isolated.

**Cost budget.** N repos = N FSW handles + N git-HEAD watchers. On Windows, ReadDirectoryChangesW scales to hundreds of handles. Not a practical limit for typical multi-repo workflows (< 20 repos).

**Failure modes.** If the spawned background task fails (e.g., FSW init error), log the error but keep `RepoState::Write { stores, .. }` — searches still work, just without live updates. Do NOT degrade to Readonly, because the DB is still writable; it's the watcher that failed.

**Interaction with `--register`.** Repos passed via `--register` at startup should get watchers on first query (same lazy-open path). Don't pre-open.

### get_or_open_stores — discriminate error types

Current code treats all failures as `Conflicted`. Differentiate:

```rust
pub(crate) fn get_or_open_stores(&self, alias: &str) -> Result<Arc<SharedStores>, String> {
    self.reload_if_changed().ok();  // best-effort — don't fail lookup on reload error

    // Fast path: already opened
    if let Some(entry) = self.repos.get(alias) {
        return match entry.value() {
            RepoState::Write { stores, .. } | RepoState::Readonly { stores } => Ok(stores.clone()),
            RepoState::Conflicted => Err(conflicted_msg(alias)),
        };
    }

    // Resolve alias → path
    let path = {
        let cfg = self.config.read().unwrap();
        cfg.resolve(alias)
    }.ok_or_else(|| format!("Unknown alias '{}'.", alias))?;

    let db_path = path.join(DB_DIR_NAME);

    // NEW: db-exists precheck — do not cache as Conflicted
    if !db_path.exists() {
        return Err(format!(
            "Database not found at {}. This usually means the repo was removed externally. \
             Run `codesearch index add {}` to recreate, or `codesearch index rm {}` to clean up the config entry.",
            db_path.display(), path.display(), path.display()
        ));
    }

    // Try write, then readonly, then Conflicted
    // ... existing flow, but create Write/Readonly variants instead of always Open
}

fn conflicted_msg(alias: &str) -> String {
    format!(
        "Repo '{}' is currently locked by another codesearch process with write access. \
         Stop that process (or let it finish) and retry. If you only need read access, \
         the lookup will retry automatically on the next query.",
        alias
    )
}
```

**Important:** a "not found" error must NOT be cached in the DashMap. Return the error, let the user fix it, and the next lookup re-evaluates. Only cache `Conflicted` because the lock state is persistent until the other process releases.

### Actionable version-mismatch error

In `McpProxy::check_health`, on version mismatch:

```rust
return Err(anyhow!(
    "codesearch serve version mismatch: serve at {} reports v{}, this binary is v{}. \
     To fix: (1) stop the running `codesearch serve`, (2) install the matching binary version on both endpoints, \
     (3) restart serve. Until versions match, MCP clients cannot connect through proxy mode.",
    base_url, their_version, env!("CARGO_PKG_VERSION")
));
```

### Remove deprecated MCP tool aliases

Delete these `#[tool]` methods from `src/mcp/mod.rs`:
- `semantic_search`, `literal_search` (use `search` instead)
- `find_definition`, `find_usages`, `find_references`, `find_imports`, `find_dependents` (use `find`)
- `file_outline`, `similar_chunks` (use `explore`)
- `index_status`, `list_projects`, `find_databases` (use `status`)

Update the `instructions` string in `ServerHandler::get_info` to drop the deprecated aliases table. Down from 17 tools to 5 — less cognitive load for the LLM, fewer misroutes.

Internal helpers like `find_usages_impl` (if any of them only serve a deprecated alias) can go away with the aliases. Helpers shared by the unified tools (e.g. the body of `semantic_search`) should be extracted into plain functions and called from `search(mode=...)`.

**Migration note for README:** no deprecated-aliases table anymore. Just document the 5 primary tools cleanly.

### Remove orphaned `src/server/` module

The old axum REST API from the demongrep fork (`pub mod server;` in `lib.rs`) is not wired to any CLI command and is not referenced outside the module itself. Delete:
- `src/server/mod.rs`
- `pub mod server;` line in `src/lib.rs`

Search the codebase for any stray references before deleting, but there shouldn't be any. This reduces build times and removes confusion between `src/server/` and `src/serve/`.

---

## File-by-file plan

### `src/mcp/mod.rs`

1. Define `RepoContext`; add `resolve_contexts` on `CodesearchService`.
2. Move `with_vector_store_read` / `with_fts_store_read` onto `impl RepoContext`.
3. Add proxy-forward guard to every tool handler (generic pattern).
4. Route every tool via `resolve_contexts` → `RepoContext` helpers.
5. Delete all `let _project = ...` no-ops.
6. Add `rrf_merge_by_rank` pure helper.
7. Add `prefix_path_with_alias` pure helper (Windows-safe via `normalize_path_str`).
8. Fan-out + merge logic for group-capable tools.
9. Single-repo tools reject `group` with clear error.
10. Extend `list_projects` serve-mode path to use live `serve_state` lock status + readonly flag.
11. **Delete deprecated tool aliases** (semantic_search, literal_search, find_definition, find_usages, find_references, find_imports, find_dependents, file_outline, similar_chunks, index_status, list_projects-as-tool (keep as internal helper if used), find_databases).
12. Update `get_info` instructions: only the 5 primary tools, no deprecated table.

### `src/cli/mod.rs`

1. Remove `ReposCommands` enum and the `Repos` variant in `Commands`.
2. Add `--alias: Option<String>` to `IndexCommands::Add`.
3. Add `--keep-config: bool` to `IndexCommands::Remove`.
4. Rewrite `IndexCommands::List` to read `ReposConfig`.

### CLI dispatch handler

1. `Index::Add`: run indexer + `register_with_alias` + save (guarded as above).
2. `Index::Remove`: `unregister_path` (unless `--keep-config`) + delete DB directory.
3. `Index::List`: read `ReposConfig`, format table + groups section + footer.
4. Remove `Commands::Repos { .. }` arm entirely.

### `src/serve/mod.rs`

1. Replace `RepoState::Open { stores }` with three-variant `{ Write, Readonly, Conflicted }`.
2. Add `config: RwLock<ReposConfig>` and `config_mtime: RwLock<Option<SystemTime>>` to `ServeState`.
3. Implement `reload_if_changed` including per-repo cancel_token firing on removal.
4. `get_or_open_stores`:
   - Precheck `db_path.exists()` → specific "not found" error, NOT cached
   - On write success: create IndexManager + cancel_token, spawn FSW + refresh task, store `Write`
   - On readonly success: store `Readonly`
   - On both fail: store `Conflicted`
5. `aliases()` and `resolve_alias` also call `reload_if_changed`.

### `src/mcp/proxy.rs`

1. Version-mismatch error now includes the actionable multi-step fix.

### `src/lib.rs`

1. Remove `pub mod server;` line.

### Files to delete

- `src/server/mod.rs` (whole module — orphaned REST API)
- (Optional) Empty `src/server/` directory — git won't track it but clean up locally.

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
- `rrf_merge_by_rank_pure`: deterministic unit test, `(alias, chunk_id)` dedup key honored
- `path_prefix_with_alias_windows`: inputs with backslashes, UNC prefixes, and mixed separators all normalize to clean forward-slash output
- `path_prefix_with_alias_preserves_none`: alias=None → path unchanged (except normalization)

**Serve + proxy:**
- `health_endpoint_live`: spawn serve on ephemeral port, GET `/health`, assert JSON shape + version
- `version_mismatch_errors_actionable`: mock different version → `check_health` returns `Err` with message containing "stop" and "restart"
- `stdio_fallback_when_no_serve`: `McpProxy::check_health(nonexistent_url) → Ok(false)`

**Lock + discrimination:**
- `lock_invariant_windows`: two processes, one DB, assert exactly one write-opens — `#[cfg(target_os = "windows")]`
- `conflicted_repo_isolated`: stdio holds write-lock on X; serve tries X → Conflicted cached; other repos still work
- `missing_db_not_cached_as_conflicted`: config references path but DB missing → specific "not found" error, not cached, next call with DB present succeeds
- `not_found_error_mentions_fix_commands`: message contains "codesearch index add" or "codesearch index rm"

**Per-repo file watchers:**
- `serve_watcher_picks_up_file_change`: spawn serve, open repo A (write mode), touch a file under A, wait for debounce, assert new chunk appears in subsequent `search` call
- `serve_watcher_handles_git_head_change`: simulate branch switch by modifying `.git/HEAD` in a fixture, assert refresh triggers
- `serve_readonly_repo_has_no_watcher`: open a repo in readonly mode (write lock held externally), assert no FSW task spawned, `list_projects` reports lock_status=readonly
- `serve_removed_alias_cancels_watcher`: open repo, remove from `repos.json`, call `get_or_open_stores` to trigger reload, assert the spawned watcher task exits (via cancel_token) within 1 second

**Cross-repo:**
- `cross_repo_search_group`: 2 indexed repos, `group="both"` → results from both, alias-prefixed, no duplicate `(alias, chunk_id)`
- `single_repo_tools_reject_group`: `file_outline`, `get_chunk`, `explore(kind="similar")`, `find(kind="imports")` with `group=Some(...)` → error

**CLI consolidation:**
- `cli_no_repos_subcommand`: `codesearch repos --help` returns CLI error
- `index_add_auto_registers`: temp dir, `index add` → entry in `repos.json`; second call → `Already registered`, no duplicate
- `index_add_with_explicit_alias`: `index add /tmp/x --alias myrepo` → alias `myrepo` in config
- `index_add_alias_collision`: existing alias `foo` → `index add /tmp/b --alias foo` → error, no write
- `index_add_global_skips_register`: `--global` → `repos.json` unchanged, warning emitted
- `index_rm_auto_unregisters`: registered repo in group → `index rm` → alias gone from `repos.json` and from group; empty group dropped
- `index_rm_preserves_config_with_flag`: `--keep-config` → DB deleted, config entry remains
- `index_rm_unregistered_path_ok`: unregistered path → no error, DB deleted
- `index_list_shows_registered_repos`: 2 repos + 1 group → all appear

**Config reload:**
- `config_reload_picks_up_new_alias`: ServeState running, append repo B to `repos.json` → `aliases()` includes B, `get_or_open_stores("B")` succeeds
- `config_reload_drops_removed_alias`: remove repo B → `get_or_open_stores("B")` errors, entry removed from DashMap, watcher cancelled
- `config_reload_no_spurious_reload`: two calls without touching config → `ReposConfig::load` called at most once (use atomic counter)
- `config_reload_concurrent_with_tool_call`: tool call on alias X in task A, reload removing X in task B with `tokio::sync::Barrier` ordering, assert task A completes successfully and its `Arc<SharedStores>` keeps LMDB open until it drops

**Cleanup:**
- `server_module_deleted`: `src/server/mod.rs` no longer exists
- `server_module_not_referenced`: `grep -rn "crate::server" src/` returns empty (verify via CI, not just a Rust test)
- `no_deprecated_tool_aliases`: `codesearch mcp` server advertises exactly 5 tools (assert via `get_info` or an integration test)

All existing tests must still pass, including `test_mcp_no_raw_stdout_calls`.

---

## Implementation order

One commit per step. Compile and test after each.

1. `RepoContext` + `resolve_contexts` — pure helpers, unit-test first
2. `prefix_path_with_alias` with Windows-safe normalization + unit tests
3. Route `index_status` (or its replacement `status(kind="index")`) end-to-end — smoke-test manually: `serve --register . && mcp && status`
4. Remaining single-repo handlers routed via `RepoContext`
5. `rrf_merge_by_rank` + fan-out
6. Group-capable handlers
7. Single-repo-tool rejection for `group`
8. `list_projects` serve-mode live lock status (includes readonly signal)
9. CLI consolidation: remove `ReposCommands`, add `--alias`/`--keep-config`, auto-register in Add, auto-unregister in Remove, rewrite List
10. `get_or_open_stores` error discrimination (db-exists precheck, actionable messages)
11. Actionable `version_mismatch` error in `McpProxy`
12. `reload_if_changed` in `ServeState`
13. **Per-repo file watchers**: `RepoState` three-variant split, spawn IndexManager + cancel_token on write open, fire on removal
14. Remove deprecated MCP tool aliases; update `get_info` instructions
15. Delete `src/server/`; remove `pub mod server;`
16. README update pass
17. Acceptance tests

Steps 1–9 can largely be done in sequence without touching serve internals. 10–13 are the serve-side changes. 14–15 are cleanup that should be left for last so the agent isn't fighting missing code earlier. 16–17 close the loop.

---

## README update (required as part of this branch)

**Remove / replace:**
- `codesearch repos add/rm/list` from "Other Commands" and "Repository Management"
- Old individual tool names as primary — new primary is `search`/`find`/`explore`/`get_chunk`/`status`
- The entire "deprecated aliases" table (aliases are gone)

**Rewrite "Repository Management" → "Index & Project Management":**
- `index add <path> [--alias NAME]` — create index AND register
- `index rm  <path>` — remove index AND unregister (symmetric with add)
- `index list` — all registered repos + state + groups
- Note: re-indexing (`codesearch index`, no `add`) leaves config alone

**Add "Groups" section:**
> Groups let you search across multiple related repos in a single call — useful for refactoring across a shared library and its consumers, or finding where a symbol is used across a whole platform.
>
> Groups are created manually by you. AI agents don't create them — only the user knows which repos belong together.
```bash
codesearch groups add platform --aliases shared-lib service-a service-b
codesearch groups list
codesearch groups rm platform
```
> In an AI session: `search(mode="semantic", group="platform", query="where is X used?")` fans out across all repos in the group and returns merged, alias-prefixed results (e.g. `shared-lib/src/auth.rs:42`).

**Update "MCP Serve Mode" section:**
- Proxy auto-detect explained clearly: stdio MCP probes `/health` at startup; if serve is running → proxy mode; if not → stdio mode
- Version mismatch → hard error with actionable message (not silent)
- Dead-session behavior: serve dies mid-session → all subsequent calls error (client must reconnect after restarting serve)
- **Live file watching in serve mode:** each opened repo gets its own file watcher + git HEAD watcher, so file edits and branch switches are picked up automatically — no manual re-index needed
- **Config reload:** `repos.json` changes are picked up on the next tool call, no serve restart needed

**Update "MCP Tools" table (primary only, 5 rows):**
| Tool | Parameters | Description |
|---|---|---|
| `search` | `query`, `mode` ("semantic" or "literal"), `limit`, `compact`, `filter_path`, `project`, `group` | Unified code search |
| `find` | `kind` (definition/usages/imports/dependents), `symbol`, `limit`, `project` | Symbol navigation |
| `explore` | `kind` (outline/similar), `target`, `limit`, `project` | File exploration |
| `get_chunk` | `chunk_id`, `context_lines`, `project` | Read chunk by ID |
| `status` | `kind` (index/projects) | Index + project info |

**Update "Other Commands" table:**
```
codesearch index add [PATH] [--alias NAME]   Create index AND register
codesearch index rm  [PATH] [--keep-config]  Remove index AND unregister
codesearch index list                          Show all registered repos + groups
codesearch groups add <n> --aliases A B...   Create/update a group
codesearch groups rm  <n>                    Remove a group
codesearch groups list                         List all groups
```

**Troubleshooting — add rows:**
| Running `codesearch index` hangs or errors during a serve session | Serve holds a write lock. Either stop serve first, or let serve's built-in file watcher handle the update automatically — it picks up changes without your intervention |
| New repo added with `index add` not visible to running serve | No action needed — serve detects `repos.json` changes on the next query |
| Serve shows "database not found" for a registered repo | The `.codesearch.db/` was removed externally. Run `codesearch index add <path>` to recreate, or `codesearch index rm <path>` to clean up |

---

## Out of scope

- Remote serve (non-localhost) — localhost-only stays
- HTTP authentication / OAuth
- Auto-start of serve from mcp (forking) — explicitly rejected
- Indexer-side dedup of stale chunks — separate future branch
- Non-AST file indexing (Markdown, YAML, configs)
- Import-graph persistence as separate structure
- Query expansion
- File-watcher-based config reload — mtime-check is sufficient
- `index rename` command — edit `repos.json` or `rm` + `add --alias`
- Restoring `repos` as a deprecated CLI alias — clean removal only
- Restoring the deprecated MCP tool aliases — same
