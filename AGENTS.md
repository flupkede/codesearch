# AGENTS.md — `feature/mcp-multi-repo` (continuation)

Single authoritative instruction file for OpenCode / Claude Code on this branch. The routing backbone is in place; what follows is the finish-line work.

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

## Code Style (keep to these throughout)

- `use crate::` for internal modules. Group imports: std → external → internal.
- `anyhow::Result<T>` from fallible functions. Never `.unwrap()` / `.expect()` in library code.
- **All error messages must be actionable** — tell the user which command to run or what to change. "X failed" without a fix is a bug.
- `PathBuf` / `&Path` for paths, `String` / `&str` for text (prefer `&str` in args).
- **Windows path hygiene**: always route through `crate::cache::normalize_path_str` before comparing, prefixing, or stripping paths. Never build paths via raw `format!` when inputs may contain backslashes.
- `tokio::spawn` + `tokio::sync::RwLock` for async shared state.
- Deterministic tests only: `tokio::sync::Barrier` for concurrency, not `sleep`.
- `tracing::{debug, info, warn}` for logs. No `print!`/`println!` in `src/mcp/`.

---

## What's already in place (do NOT redo)

- `validate_project_group` helper in `src/mcp/types.rs` + 8 tests
- `HasChunkId` / `HasScore` traits + cross-store dedup logic + 20 tests in `src/mcp/mod.rs`
- Tool handlers route via `serve_state.get_or_open_stores(alias)` for `project`; group fan-out via `resolve_group_aliases` → loop → `get_or_open_stores`
- `resolve_group_aliases(&str)` on `ServeState`
- `serve_state: Option<Arc<ServeState>>` field on `CodesearchService`
- `src/server/` (orphaned axum REST API) deleted; `pub mod server;` removed from `src/lib.rs`
- Consolidated tool surface (`search`/`find`/`explore`/`get_chunk`/`status`) + deprecated aliases still present

Leave these alone. If you refactor any of them, limit to extracting shared helpers — no behavior changes.

---

## What remains (your work)

The 13 gaps below are ordered by value + risk. Do them in order, one commit per step, tests after each.

### 1. Extract the routing preamble into a helper (removes duplication)

Every routed handler currently repeats this block:

```rust
let serve_state = self.serve_state.as_ref()
    .ok_or_else(|| "project/group routing requires `codesearch serve` to be running.".to_string())?;
types::validate_project_group(project, group, true)?;
if let Some(ref alias) = project {
    let stores = serve_state.get_or_open_stores(alias)?;
    // ...
} else if let Some(ref group_name) = group {
    let aliases = serve_state.resolve_group_aliases(group_name)?;
    // ...
}
```

Extract to `CodesearchService::resolve_stores(project, group) -> Result<Vec<(String, Arc<SharedStores>)>, String>` returning `(alias, stores)` pairs — length 1 for single project, N for group. Stdio mode (no serve_state, both None) returns `vec![(String::new(), self.shared_stores.clone().unwrap())]`. Any other stdio combination → error.

Then every handler shrinks to:
```rust
let targets = self.resolve_stores(project.as_deref(), group.as_deref())?;
// single-repo tool: reject targets.len() > 1 with "tool X operates on a single repo"
// multi-repo tool: loop, collect, merge
```

This is pure refactor — no behavior change, just cleanup. Tests: the existing handler tests keep passing.

### 2. `prefix_path_with_alias` Windows-safe helper

Pure function in `src/mcp/mod.rs`:

```rust
pub(crate) fn prefix_path_with_alias(path: &str, alias: Option<&str>, project_root: &str) -> String {
    let normalized = crate::cache::normalize_path_str(path);
    let normalized_root = crate::cache::normalize_path_str(project_root)
        .trim_end_matches('/').to_string();
    let relative = normalized.strip_prefix(&normalized_root)
        .unwrap_or(&normalized).trim_start_matches('/');
    match alias {
        Some(a) if !a.is_empty() => format!("{}/{}", a, relative),
        _ => normalized,
    }
}
```

Apply everywhere a result path is returned from serve mode — both single-project and group. In stdio mode (alias empty), the function still normalizes (good) but doesn't prefix.

Tests:
- `path_prefix_windows_backslashes`: `"C:\\repo\\src\\main.rs"`, root `"C:\\repo"`, alias `"myrepo"` → `"myrepo/src/main.rs"`
- `path_prefix_unc_prefix`: `"\\\\?\\C:\\repo\\src\\main.rs"` handled
- `path_prefix_mixed_separators`: input with both `/` and `\` → clean forward-slash output
- `path_prefix_no_alias`: alias `None` → path normalized, no prefix

### 3. Remove deprecated MCP tool aliases (17 → 5 tools)

Delete these `#[tool]` methods from `src/mcp/mod.rs`:
- `semantic_search`, `literal_search`
- `find_definition`, `find_usages`, `find_references`, `find_imports`, `find_dependents`
- `file_outline`, `similar_chunks`
- `index_status`, `list_projects` (as a tool; keep as internal helper if `status(kind="projects")` delegates to it), `find_databases`

Internal implementation functions (e.g. the body that `semantic_search` used to run) should be kept as plain `async fn` helpers — the unified tools `search(mode=...)` and friends already delegate to them.

Update the `instructions` string in `ServerHandler::get_info` — drop the "Deprecated aliases" table entirely. Keep only the 5 primary tools.

Test: `no_deprecated_tool_aliases` — integration-style, confirms `get_info()` / tool registry exposes exactly 5 tools.

### 4. CLI consolidation — remove `repos` subcommand, fold into `index`

In `src/cli/mod.rs`:
- Delete the `ReposCommands` enum (currently at line ~55) and the `Repos { command: ReposCommands }` variant from `Commands` (currently at ~296).
- Delete `Commands::Repos { .. }` arm in the dispatch (at ~456) and the `run_repos_command` function (at ~620).
- Add `alias: Option<String>` flag to `IndexCommands::Add`.
- Add `keep_config: bool` flag to `IndexCommands::Remove`.
- Rewrite the `IndexCommands::List` handler to read `ReposConfig::load()` and format a table of all registered repos + a groups section + a current-directory footer.

No deprecated `repos` alias — the subcommand has never shipped.

Final CLI:
```
codesearch index add <path> [--alias NAME] [--global]
codesearch index rm  <path> [--keep-config]
codesearch index list
codesearch groups add <n> --aliases <alias>...
codesearch groups rm  <n>
codesearch groups list
```

Test: `cli_no_repos_subcommand` asserts `codesearch repos --help` exits with a clap-error.

### 5. Auto-register in `index add`

In the `Index::Add` dispatch arm, after the DB is created:
- Canonicalize the path
- Load `ReposConfig::load()`
- If `config.alias_for_path(&canonical).is_some()` → print `ℹ️ Already registered as '{alias}'.` and skip
- Otherwise `config.register_with_alias(canonical, cli_alias)` → on collision (same alias, different path) return a CLI error; on success `config.save()` and print `✅ Registered as '{alias}'.`
- `--global`: skip register, warn clearly that global indexes are not discoverable by serve
- On `repos.json` write failure: warn but do NOT fail the indexing op

Tests:
- `index_add_auto_registers`
- `index_add_with_explicit_alias`
- `index_add_alias_collision`
- `index_add_already_registered_noop`
- `index_add_global_skips_register`

### 6. Auto-unregister in `index rm`

In the `Index::Remove` dispatch arm:
- Canonicalize path
- Unless `--keep-config`: `ReposConfig::load() → unregister_path(&canonical) → save()`. The existing `unregister_path` handles group cleanup (drops from groups, drops empty groups).
- If path wasn't registered → silent skip, proceed with DB deletion
- Delete the `.codesearch.db/` directory
- Windows "file in use" error → warn the user to stop serve first

Tests:
- `index_rm_auto_unregisters`
- `index_rm_preserves_config_with_flag`
- `index_rm_unregistered_path_ok`
- `index_rm_group_cleanup` — repo was in a group, after rm the group no longer contains it, empty group is dropped

### 7. `get_or_open_stores` — db-exists precheck + actionable errors

In `src/serve/mod.rs`, before trying `SharedStores::new`:

```rust
if !db_path.exists() {
    return Err(format!(
        "Database not found at {}. This usually means the repo was removed externally. \
         Run `codesearch index add {}` to recreate, or `codesearch index rm {}` to clean up the config entry.",
        db_path.display(), path.display(), path.display()
    ));
}
```

**Do NOT cache this as `Conflicted`** — return the error and let the next lookup re-evaluate. Conflicts get cached (the lock persists); missing DBs are a fix-it-and-retry situation.

Update the conflict error message to actually say what to do:

```rust
fn conflicted_msg(alias: &str) -> String {
    format!(
        "Repo '{}' is currently locked by another codesearch process with write access. \
         Stop that process (or let it finish) and retry. If you only need read access, \
         the next query will retry automatically.",
        alias
    )
}
```

Tests:
- `missing_db_not_cached_as_conflicted` — remove DB directory between calls, assert first returns "not found", after recreating DB a second call succeeds without manual intervention
- `not_found_error_mentions_fix_commands` — error text contains `codesearch index add` or `codesearch index rm`

### 8. Actionable version-mismatch error in `McpProxy::check_health`

Current text: `"codesearch serve version mismatch: serve={} mcp={}."` — not actionable.

Replace with:
```rust
return Err(anyhow!(
    "codesearch serve version mismatch: serve at {} reports v{}, this binary is v{}. \
     To fix: (1) stop the running `codesearch serve`, (2) install the matching binary version on both endpoints, \
     (3) restart serve. Until versions match, MCP clients cannot connect through proxy mode.",
    base_url, their_version, env!("CARGO_PKG_VERSION")
));
```

Test: `version_mismatch_errors_actionable` — mock `/health` returning a different version, assert `check_health` returns `Err` whose message contains "stop" and "restart".

### 9. Config reload on demand in `ServeState`

Change `ServeState` to:
```rust
pub(crate) struct ServeState {
    repos: DashMap<String, RepoState>,
    config: RwLock<ReposConfig>,
    config_mtime: RwLock<Option<SystemTime>>,
    dimensions_cache: DashMap<String, usize>,
}
```

Private helper `reload_if_changed(&self) -> Result<()>`:
1. `stat` `config_path()?`. Equal mtime → no-op.
2. On change: `ReposConfig::load()`. Parse error → log, keep old config, still update mtime (avoids retry storm on a broken file).
3. Compute removed aliases = `old.repos.keys() - new.repos.keys()`.
4. For each removed alias: collect (do not hold the write lock while removing from DashMap). Fire its cancel_token if Write (see §10). Then `self.repos.remove(alias)`. Drop chain closes LMDB, stops FSW.
5. Replace `self.config` (write lock), update `self.config_mtime`.
6. Do NOT pre-open newly-added aliases — lazy.

Call it at the top of: `get_or_open_stores`, `aliases`, `resolve_alias`, `resolve_group_aliases`.

Tests:
- `config_reload_picks_up_new_alias` — append entry to `repos.json` after `ServeState::new`, next `aliases()` sees it, `get_or_open_stores` succeeds
- `config_reload_drops_removed_alias` — rewrite `repos.json` without X, assert `get_or_open_stores("X")` errors and DashMap no longer holds X
- `config_reload_no_spurious_reload` — atomic counter on `ReposConfig::load` calls; two lookups without mtime change → counter still 1
- `config_reload_concurrent_with_tool_call` — `tokio::sync::Barrier`-ordered: task A holds a clone of the `Arc<SharedStores>` for X while task B triggers a reload that removes X. Task A completes successfully; its Arc keeps LMDB open until drop.

### 10. Per-repo file watchers in serve (Option A)

This is the biggest piece. Without it serve is functionally broken for active development.

Extend `RepoState`:
```rust
pub(crate) enum RepoState {
    Write {
        stores: Arc<SharedStores>,
        #[allow(dead_code)]
        index_manager: Arc<IndexManager>,  // kept alive for its watcher task
        cancel_token: CancellationToken,
    },
    Readonly { stores: Arc<SharedStores> },
    Conflicted,
}
```

Update the fast-path match in `get_or_open_stores`:
```rust
RepoState::Write { stores, .. } | RepoState::Readonly { stores } => Ok(stores.clone()),
```

After a successful **write** open of `SharedStores`:
1. `let index_manager = Arc::new(IndexManager::new_without_refresh(&project_path, stores.clone()).await?);`
2. `let cancel_token = CancellationToken::new();`
3. `tokio::spawn` a task that:
   - Calls `index_manager.start_watching().await` (starts FSW pre-collection so changes during initial refresh aren't missed)
   - Runs `IndexManager::perform_incremental_refresh_with_stores(&project_path, &db_path, &stores).await`
   - Checks `cancel_token.is_cancelled()` before continuing
   - Starts the file watcher via `index_manager.start_file_watcher(cancel_token.clone()).await`
   - Logs any failure but does not crash the server
4. Store `RepoState::Write { stores: Arc::clone, index_manager, cancel_token }` in DashMap

After a successful **readonly** open → `RepoState::Readonly { stores }`. No watcher, no IndexManager.

On reload removal: if Write, fire the token. If Readonly, just drop. The spawned task exits when it sees `cancel_token.is_cancelled()` either between steps or via the `tokio::select!` inside `start_file_watcher`.

**Failure policy:** if `IndexManager::new_without_refresh` errors, log it and still store `RepoState::Write { stores, .. }` with a dummy cancel_token — searches still work, just without live updates. Do NOT degrade the state to `Readonly` because the DB itself is writable; it's the watcher that failed. (If degrading to Readonly feels cleaner to the agent, that's acceptable too — the key thing is not to error out the whole query.)

Tests:
- `serve_watcher_picks_up_file_change` — open a repo in write mode, touch a file under project_path, wait for debounce window, assert a subsequent `search` call returns the new content
- `serve_watcher_handles_git_head_change` — modify `.git/HEAD` in the fixture, assert refresh triggered
- `serve_readonly_repo_has_no_watcher` — hold write lock externally, serve opens Readonly, assert no spawned task and `list_projects` reports `lock_status="readonly"`
- `serve_removed_alias_cancels_watcher` — after config reload removes the alias, the spawned task exits within 1 second (check via a separately-held handle to the cancel_token or via a completion signal channel)

### 11. `list_projects` — live lock status from serve_state

When `self.serve_state` is `Some`, iterate `serve_state.repos` (after `reload_if_changed`) and derive `lock_status`:
- `RepoState::Write { .. }` → `"write"`
- `RepoState::Readonly { .. }` → `"readonly"`
- `RepoState::Conflicted` → `"conflicted"`
- Not in DashMap (never queried) → fall back to `is_database_locked(&db_path)` → `"available"` or `"locked-externally"`

Test: `list_projects_reports_readonly` — set up a fixture where repo B is readonly-opened in serve, assert the response shows B as readonly.

### 12. README update

Remove:
- All `codesearch repos add/rm/list` rows from tables and the "Repository Management" section
- The "deprecated aliases" table (no aliases anymore)

Rewrite "Repository Management" → "Index & Project Management":
```
codesearch index add [PATH] [--alias NAME] [--global]
codesearch index rm  [PATH] [--keep-config]
codesearch index list
```

Add a new "Groups" section:
> Groups let you search across multiple related repos in a single call — useful for refactoring across a shared library and its consumers, or finding where a symbol is used across a whole platform.
>
> Groups are created manually by you. AI agents don't create them — only you know which repos belong together.
> ```
> codesearch groups add platform --aliases shared-lib service-a service-b
> codesearch groups list
> codesearch groups rm platform
> ```
> In an AI session: `search(mode="semantic", group="platform", query="where is X used?")` fans out across all repos in the group and returns merged, alias-prefixed results (e.g. `shared-lib/src/auth.rs:42`).

Update "MCP Serve Mode" section:
- Proxy auto-detect: stdio MCP probes `/health` at startup; if serve is running → proxy mode; if not → stdio mode
- Version mismatch → hard error (actionable message)
- Dead-session: serve dies mid-session → all subsequent calls error; client must reconnect
- **Live file watching:** each opened repo gets its own FSW + git-HEAD watcher; file edits and branch switches are picked up automatically
- **Config reload:** `repos.json` changes picked up on the next tool call, no restart needed

Update "MCP Tools" table to 5 primary rows (search / find / explore / get_chunk / status). Remove the aliases table.

Add troubleshooting rows:
- `codesearch index` hangs during a serve session → serve owns the write lock; let its built-in watcher handle updates, or stop serve first
- New repo invisible to running serve → no action; serve detects on next query
- `serve shows "database not found"` → the `.codesearch.db/` was removed externally; run `index add` to recreate or `index rm` to clean up

### 13. Full acceptance pass

- `cargo test --all` passes
- `cargo clippy --all-targets -- -D warnings` passes
- Manual smoke script in the PR description (see below)

---

## Acceptance criteria summary

Every test named in §1–§11 must exist and pass. In addition, these previously-written tests must still pass:
- `test_mcp_no_raw_stdout_calls`
- All `validate_project_group` tests in `types.rs`
- All `HasChunkId` / `HasScore` / multi-store dedup tests in `mcp/mod.rs`

---

## Manual-test script (for PR description)

```bash
# Setup: two repos + a group
codesearch index add /tmp/repo-a                   # → "Indexed … as 'repo-a'"
codesearch index add /tmp/repo-b --alias bravo     # → "Indexed … as 'bravo'"
codesearch index list                              # → shows both, no groups yet
codesearch groups add pair --aliases repo-a bravo
codesearch groups list                             # → "pair → [repo-a, bravo]"

# Serve + MCP
codesearch serve --port 39725 &
# From MCP client:
#   status(kind="projects")  → shows repo-a + bravo + group pair
#   search(mode="semantic", project="repo-a", query="...")  → alias-prefixed results
#   search(mode="semantic", group="pair", query="...")      → results from both, merged

# Hot add — no restart
codesearch index add /tmp/repo-c                   # serve picks it up on next query
#   status(kind="projects")  → now shows repo-c

# Hot rm — no restart, group cleanup
codesearch index rm /tmp/repo-a
#   search(project="repo-a", ...)  → "Unknown alias"
#   status(kind="projects")        → pair now contains only bravo (or was dropped)

# Live file watching
echo "// touched" >> /tmp/repo-b/src/something.rs
# Wait ~2 seconds for debounce
#   search(project="bravo", query="touched")  → returns the new content
```

---

## Out of scope

- Remote serve (non-localhost) — localhost-only
- HTTP authentication / OAuth
- Auto-start of serve from mcp (forking) — rejected
- Indexer-side dedup of stale chunks — separate future branch
- Non-AST file indexing (Markdown, YAML, configs)
- Query expansion, import-graph persistence
- File-watcher-based config reload — mtime check is sufficient
- `index rename` — edit `repos.json` or `rm` + `add --alias`
