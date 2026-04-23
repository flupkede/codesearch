# AGENTS.md — `feature/mcp-multi-repo` (continuation)

Single authoritative instruction file for OpenCode / Claude Code on this branch. Routing backbone is in place; this file covers the remaining finish-line work.

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

### MCP-only rule for the agent
**ONLY USE MCP TOOLS** for code exploration and editing. Only use bash for `cargo build`, `cargo test`, `cargo clippy`, and the index-list command above.

---

## Code Style

- `use crate::` for internal modules. Group imports: std → external → internal.
- `anyhow::Result<T>` from fallible functions. Never `.unwrap()` / `.expect()` in library code.
- Mutex: `.lock().map_err(|e| anyhow::anyhow!("Mutex poisoned: {}", e))?`
- **All error messages must be actionable.** Tell the user which command to run or what to change. "X failed" without a fix is a bug.
- `PathBuf` / `&Path` for paths, `String` / `&str` for text (prefer `&str` in args).
- **Windows path hygiene**: always route through `crate::cache::normalize_path_str` before comparing, prefixing, or stripping paths. Never build paths via raw `format!` when inputs may contain backslashes.
- `tokio::spawn` + `tokio::sync::RwLock` for async shared state.
- Deterministic tests only — `tokio::sync::Barrier` or explicit signals for concurrency, not `sleep`.
- `tracing::{debug, info, warn}` for logs. **No `print!`/`println!` in `src/mcp/`.** Enforced by `test_mcp_no_raw_stdout_calls`.
- `#[derive(Serialize, Deserialize)]` + `#[serde(skip_serializing_if = "Option::is_none")]` for optional fields.
- Pre-allocate `HashMap::with_capacity(size)` when size is known.
- LMDB `map_size` = 2GB. Peak memory target: ~500-700MB for large codebases.
- Graceful CTRL-C via `tokio::select!` + `tokio::signal`. Exit code 130 on SIGINT. Close DB handles before exit.

---

## Project Architecture (context for the work below)

**Serve/proxy mode.** `codesearch serve` binds `127.0.0.1:{port}` (default 39725, env `CODESEARCH_SERVE_PORT`) and exposes:
- `GET /health` → `{"codesearch_server": true, "version": "..."}`
- MCP streamable HTTP at `/mcp` via rmcp tower service

`codesearch mcp` on startup probes `/health` with 200ms timeout:
- Reachable + version matches → **proxy mode** (forwards all tool calls)
- Not reachable → stdio mode (existing behavior)
- Version mismatch → hard error

Proxy dead-session: once serve becomes unreachable mid-session, all calls return a fixed message. No reconnect, no local fallback.

**Repos config `~/.codesearch/repos.json`:**
```json
{
  "repos": { "<alias>": "<absolute-path>", ... },
  "groups": { "<group-name>": ["<alias1>", "<alias2>"], ... }
}
```

**MCP tool surface (consolidated, 5 primary):** `search` / `find` / `explore` / `get_chunk` / `status`. Currently 12 deprecated aliases are still registered — one of the remaining tasks is to delete them.

**Key constants (`src/constants.rs`):**
`DEFAULT_SERVE_PORT = 39725`, `SERVE_PORT_ENV = "CODESEARCH_SERVE_PORT"`, `HEALTH_PATH = "/health"`, `MCP_ENDPOINT_PATH = "/mcp"`, `HEALTH_PROBE_TIMEOUT_MS = 200`, `DEFAULT_EMBEDDING_DIMENSIONS = 384`.

---

## What's already in place (do NOT redo)

- `validate_project_group` helper in `src/mcp/types.rs` + 8 tests
- `HasChunkId` / `HasScore` traits + cross-store dedup + 20 tests in `src/mcp/mod.rs`
- `MultiStoreContext` + `resolve_routing()` pattern — handlers route via `serve_state.get_or_open_stores(alias)` for `project`, and `resolve_group_aliases` + loop for `group`
- `resolve_group_aliases(&str)` on `ServeState`
- `serve_state: Option<Arc<ServeState>>` field on `CodesearchService`
- `src/server/` (orphaned axum REST API) deleted
- All request types accept `project: Option<String>` and `group: Option<String>` with serde roundtrip tests
- `literal_search` end_line fix (1-line range semantics)
- Group fan-out bug fix (all members, not just first)

Tests currently passing: 286.

If you refactor any of the above, limit yourself to extracting shared helpers — no behavior changes.

---

## Remaining work — 11 items, ordered by value + risk

One commit per step. Compile + test after each. No release builds.

---

### 1. `prefix_path_with_alias` — Windows-safe helper

**Why.** Group-query results from different repos with overlapping paths (e.g. both have `src/main.rs`) are currently indistinguishable to the LLM. Adding an alias prefix makes results traceable. Windows backslashes must be normalized first to avoid mixed separators like `"myalias/src\main.rs"`.

**Where.** Add pure helper near the top of `src/mcp/mod.rs`:

```rust
/// Prefix a result path with its repo alias for group queries, normalizing
/// Windows backslashes to forward slashes in the process. When `alias` is
/// None or empty, the path is still normalized (useful for stdio mode).
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

**Apply everywhere** a result path is returned in serve mode. The relevant code paths in `src/mcp/mod.rs` all end up building `SearchResultItem`, `LiteralSearchResultItem`, `ReferenceItem`, `FileOutlineItem`, `ImportItem`, or `DependentItem` entries — each has a `path` field. For group queries, you already know which alias each set of results came from (you're looping over `resolve_group_aliases`).

For single-project queries in serve mode, still prefix with the alias (so the LLM knows which repo the result is from). In stdio mode (no serve_state), alias is None → path is just normalized, no prefix.

**Tests** (add to the `tests` module in `src/mcp/mod.rs`):
- `path_prefix_windows_backslashes`: input `"C:\\repo\\src\\main.rs"`, root `"C:\\repo"`, alias `Some("myrepo")` → `"myrepo/src/main.rs"`
- `path_prefix_unc_prefix`: input `"\\\\?\\C:\\repo\\src\\main.rs"` handled correctly
- `path_prefix_mixed_separators`: input with both `/` and `\` → clean forward-slash output
- `path_prefix_no_alias`: alias `None` → path normalized, no prefix added
- `path_prefix_empty_alias`: alias `Some("")` → no prefix, same as None
- `path_prefix_preserves_path_outside_root`: path that doesn't start with root → path unchanged except normalization

---

### 2. Remove deprecated MCP tool aliases (17 → 5)

**Why.** 12 extra tools inflate the LLM-visible surface and increase misroute/grep-fallback behavior. The `description = "DEPRECATED. Use ..."` prefix helps a bit, but agents still call them.

**Where.** `src/mcp/mod.rs`. Delete these `#[tool]` methods:
- `semantic_search`
- `literal_search`
- `find_definition`
- `find_usages`
- `find_references`
- `find_imports`
- `find_dependents`
- `file_outline`
- `similar_chunks`
- `index_status`
- `list_projects` (keep the helper body if needed; only remove the `#[tool]` wrapper)
- `find_databases`

Internal implementation functions (e.g. the body that `semantic_search` used) stay as plain `async fn` helpers. The unified tools `search(mode=...)`, `find(kind=...)`, `explore(kind=...)`, and `status(kind=...)` already delegate to them.

**Update `get_info` instructions** (also in `src/mcp/mod.rs`):
- Drop the entire "Deprecated aliases (still functional)" table from the `instructions` string
- Keep only the 5 primary tools section
- The existing test `test_instructions_max_50_lines` will still enforce the 50-line budget — should pass easily after the table is removed

**Update `types.rs`:** the deprecated request types (`SemanticSearchRequest`, `FindReferencesRequest`, `LiteralSearchRequest`, `FindDefinitionRequest`, `FindUsagesRequest`, `FileOutlineRequest`, `FindImportsRequest`, `FindDependentsRequest`, `SimilarChunksRequest`) can be deleted. Remove their `#[test]` serde-roundtrip tests along with them (these are in `src/mcp/mod.rs` tests module — names like `test_semantic_search_request_with_group`, `test_find_definition_request_with_group`, etc.).

Keep the primary types' `with_group` tests intact (`test_find_request_with_group`, `test_explore_request_with_group`, `test_status_request_with_group`, `test_search_request_with_group`, `test_get_chunk_request_with_group`).

**Tests:**
- `no_deprecated_tool_aliases`: confirms `get_info()` no longer mentions `semantic_search`, `find_references`, or any of the other deprecated names in `instructions`
- Existing tests must still pass; deprecated-request-type tests should be deleted with their types

---

### 3. CLI consolidation — remove `repos` subcommand, fold into `index`

**Why.** Two commands for the same concept (`repos add` vs `index add`) is confusing pre-1.0. Folding into `index` gives one clear entry point and enables auto-register/auto-unregister (next items). No deprecated alias — `repos` has never shipped in a release.

**Where.** `src/cli/mod.rs`:

1. Delete the `ReposCommands` enum (currently lines ~55-76)
2. Delete the `Repos { command: ReposCommands }` variant from `Commands` (currently ~line 296)
3. Delete the `Commands::Repos { command } => run_repos_command(command).await` arm (currently ~line 456)
4. Delete the entire `run_repos_command` function (currently ~lines 620-660)
5. Update the `IndexCommands::Add` variant to add `alias: Option<String>`:
   ```rust
   Add {
       path: Option<PathBuf>,
       #[arg(short = 'g', long)]
       global: bool,
       #[arg(short, long)]
       alias: Option<String>,
   }
   ```
6. Update `IndexCommands::Remove` to add `keep_config: bool`:
   ```rust
   Remove {
       path: Option<PathBuf>,
       #[arg(long)]
       keep_config: bool,
   }
   ```
7. Rewrite the `IndexCommands::List` handler to read `ReposConfig::load()` and print a table of registered repos + groups + current-directory footer

Note that the current `Commands::Index { add, global, remove, list, ... }` flag-based dispatch predates `IndexCommands`. Keep the flag-based backward-compat arm alive, but route `--add` and `--rm` through the same logic that handles `IndexCommands::Add`/`Remove` (defined in items 4 and 5 below).

**Final CLI after this step:**
```
codesearch index                               # incremental re-index of current repo
codesearch index add <path> [--alias NAME] [--global]
codesearch index rm  <path> [--keep-config]
codesearch index list
codesearch groups add <n> --aliases A B...
codesearch groups rm  <n>
codesearch groups list
```

`groups` stays as its own subcommand — it operates on aliases, not paths, so folding it into `index` would be awkward.

**Also update `run_groups_command`** — the validation error text currently says `Use 'codesearch repos add' first.`. Replace with `Use 'codesearch index add' first.`.

**Tests:**
- `cli_no_repos_subcommand`: `codesearch repos --help` exits with clap-error (e.g. `Cli::try_parse_from(["codesearch", "repos", "--help"]).is_err()`)
- `cli_index_add_accepts_alias_flag`: `codesearch index add /tmp/foo --alias myrepo` parses to `Commands::Index { add: true, alias: Some("myrepo"), .. }` (or its `IndexCommands::Add` equivalent, whichever the final dispatch uses)
- `cli_index_rm_accepts_keep_config_flag`: `codesearch index rm /tmp/foo --keep-config` parses correctly

---

### 4. Auto-register in `index add`

**Why.** With `repos` subcommand gone, `index add` becomes the single entry for both creating an index and making it discoverable by serve. Without auto-register, every user would have to manually edit `~/.codesearch/repos.json`.

**Where.** In the Index-Add dispatch (current arm `if add || is_add_cmd` in `src/cli/mod.rs`). After the indexing operation completes successfully:

1. Canonicalize the path (`PathBuf::canonicalize`, fallback to as-is on error — existing pattern in the codebase)
2. Load `ReposConfig::load()` (with `unwrap_or_default()`)
3. If `config.alias_for_path(&canonical)` returns `Some(existing_alias)`:
   - Print `ℹ️ Already registered as '{existing_alias}'.` to stderr
   - Skip the register step
4. Otherwise:
   - Call `config.register_with_alias(canonical, user_alias)` where `user_alias` is the `--alias` flag value (may be None → auto-generate from dir name)
   - On alias collision (same alias, different path) → return a CLI error with a helpful message: `Alias '{}' already used by '{}'. Choose a different --alias.`
   - On success → `config.save()?`
   - Print `✅ Registered as '{assigned_alias}'.`
5. `--global` flag: skip register entirely, print `⚠️ Global indexes are not auto-registered. Use 'index add' without --global for serve discovery.`
6. If `config.save()` fails: log a warning but do NOT fail the indexing op — the DB is valid, only discoverability is lost

**Where the helpers live.** `ReposConfig` API is in `src/db_discovery/repos.rs`. Use `alias_for_path`, `register_with_alias`, `save` — these already exist. If a helper is missing, add it there rather than duplicating logic in the CLI.

**Tests:**
- `index_add_auto_registers`: temp dir, call index-add → entry appears in `repos.json`
- `index_add_idempotent`: second call on same path → `Already registered`, no duplicate entry, no error
- `index_add_with_explicit_alias`: `--alias myrepo` → alias in config is `myrepo`
- `index_add_alias_collision`: pre-existing alias `foo` for path A; `index add /tmp/B --alias foo` → CLI error, no write to `repos.json`
- `index_add_global_skips_register`: `--global` → `repos.json` unchanged, warning on stderr

---

### 5. Auto-unregister in `index rm`

**Why.** Symmetry with auto-register. Without this, deleting a repo leaves a dangling entry in `repos.json` and potentially stale members in groups.

**Where.** In the Index-Remove dispatch (current `if remove || is_rm_cmd` arm in `src/cli/mod.rs`). Before or after the DB deletion:

1. Canonicalize path
2. Unless `--keep-config`:
   - `ReposConfig::load()` → `unregister_path(&canonical)` → `save()`
   - `unregister_path` should already handle group cleanup: drop alias from all groups, drop groups that become empty. If it doesn't, extend it.
3. If path wasn't registered: silent skip (no error), proceed with DB deletion
4. Delete `.codesearch.db/` (existing `remove_from_index` logic)
5. On Windows "file in use" error during DB deletion: warn `⚠️ Database files may be locked by a running codesearch serve. Stop it and retry.` — do not retry automatically
6. `--keep-config`: delete DB only, print `ℹ️ Config entry preserved.`

**Order of operations note.** Config write before or after DB delete? Do **DB delete first**, config write second. Rationale: if DB delete fails (Windows lock), the config stays accurate. If config write fails after DB is deleted, warn the user to clean `repos.json` manually.

**Tests:**
- `index_rm_auto_unregisters`: registered repo → rm → alias gone from `repos.json`
- `index_rm_group_cleanup`: repo was in group `foo` → rm removes alias from group; if group becomes empty, drop the group entry
- `index_rm_preserves_config_with_flag`: `--keep-config` → DB deleted, config entry remains
- `index_rm_unregistered_path_ok`: unregistered path → no error, DB deleted normally

---

### 6. `get_or_open_stores` — db-exists precheck + actionable errors

**Why.** Today, a missing DB (e.g. user removed the dir externally) is cached as `RepoState::Conflicted`, which is both wrong (it's not conflicted, it's gone) and non-recoverable without a serve restart. The error message for genuine conflicts also has no fix instructions.

**Where.** `src/serve/mod.rs`, inside `get_or_open_stores`.

Before calling `SharedStores::new`, add:

```rust
let db_path = path.join(DB_DIR_NAME);

if !db_path.exists() {
    return Err(format!(
        "Database not found at {}. This usually means the repo was removed externally. \
         Run `codesearch index add {}` to recreate, or `codesearch index rm {}` to clean up the config entry.",
        db_path.display(), path.display(), path.display()
    ));
}
```

**Critical:** do NOT insert the missing-DB error into `self.repos` as `Conflicted`. Let the error bubble up without caching; the next call re-evaluates (so if the user runs `index add` to recreate, the very next query works).

Replace the conflict error message in both places (fast-path match arm + slow-path failure arm) with:

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

And use `conflicted_msg(alias)` in both arms instead of the inlined format.

**Tests:**
- `missing_db_not_cached_as_conflicted`: set up a repo with config entry but no DB dir → `get_or_open_stores` returns "not found" error → DashMap does NOT contain the alias → recreate DB → next call succeeds without restart
- `not_found_error_mentions_fix_commands`: error text contains both `codesearch index add` and `codesearch index rm`
- `conflicted_error_mentions_stop_and_retry`: error text contains "Stop" and "retry"

---

### 7. Actionable version-mismatch error in `McpProxy::check_health`

**Why.** Current text is `"codesearch serve version mismatch: serve={} mcp={}. Restart serve or update the mcp binary."` — this doesn't tell the user which one to update or in what order.

**Where.** `src/mcp/proxy.rs`, inside `check_health`, replace the existing `anyhow!` with:

```rust
return Err(anyhow::anyhow!(
    "codesearch serve version mismatch: serve at {} reports v{}, this binary is v{}. \
     To fix: (1) stop the running `codesearch serve`, (2) install the matching binary version on both endpoints, \
     (3) restart serve. Until versions match, MCP clients cannot connect through proxy mode.",
    base_url, body.version, my_version
));
```

The existing call site already has `base_url` in scope via the function parameter. If not passed through, adjust the signature.

**Test:**
- `version_mismatch_errors_actionable`: spin up a tiny test HTTP server that returns `{"codesearch_server": true, "version": "0.0.1"}` while the binary is at a different version, call `McpProxy::check_health`, assert `Err` whose message contains all of: "stop", "install", "restart"

---

### 8. Config reload on demand in `ServeState`

**Why.** With auto-register in `index add`, a new repo added from another terminal must be visible to the running serve on the next query. Without reload, the alias stays unknown until serve is restarted. Using mtime-check on each lookup is race-safe and avoids the complexity of a file watcher.

**Where.** `src/serve/mod.rs`. Change `ServeState`:

```rust
pub(crate) struct ServeState {
    repos: DashMap<String, RepoState>,
    config: std::sync::RwLock<ReposConfig>,
    config_mtime: std::sync::RwLock<Option<std::time::SystemTime>>,
}
```

Add private helper:

```rust
fn reload_if_changed(&self) -> anyhow::Result<()> {
    let config_path = ReposConfig::path()?;  // existing ReposConfig helper or add it
    let mtime = match std::fs::metadata(&config_path).and_then(|m| m.modified()) {
        Ok(m) => Some(m),
        Err(_) => None,  // file doesn't exist yet — treat as no change
    };

    let current_mtime = *self.config_mtime.read().unwrap();
    if mtime == current_mtime {
        return Ok(());  // no change
    }

    // Load new config; on parse error, keep old config but update mtime to avoid retry storm
    let new_config = match ReposConfig::load() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Failed to reload repos config: {}. Keeping current config.", e);
            *self.config_mtime.write().unwrap() = mtime;
            return Ok(());
        }
    };

    // Compute removed aliases under read lock (don't hold it long)
    let removed: Vec<String> = {
        let old = self.config.read().unwrap();
        old.repos.keys()
            .filter(|k| !new_config.repos.contains_key(*k))
            .cloned()
            .collect()
    };

    // For each removed alias: fire its cancel_token (see item 9), then remove from DashMap.
    // Drop order matters — fire first, remove second, so the spawned FSW task sees cancellation
    // before its RepoState drops.
    for alias in &removed {
        if let Some((_, state)) = self.repos.remove(alias) {
            if let RepoState::Write { cancel_token, .. } = state {
                cancel_token.cancel();
            }
            // Readonly and Conflicted just drop
        }
    }

    // Swap in the new config
    *self.config.write().unwrap() = new_config;
    *self.config_mtime.write().unwrap() = mtime;

    Ok(())
}
```

**Note about item 9 ordering:** the `RepoState::Write { cancel_token, .. }` pattern above references the three-variant enum from item 9. Do item 9 **before** completing the cancel_token wiring here. For this item alone, you can match against today's two-variant enum:

```rust
if let Some((_, _state)) = self.repos.remove(alias) {
    // state drops here — LMDB closes, FSW stops
}
```

Then re-visit after item 9 to add the cancel-token fire.

**Call `reload_if_changed` at the top of:**
- `get_or_open_stores`
- `aliases`
- `resolve_group_aliases`

Don't fail the lookup on reload error — use `.ok()` or log-and-continue.

**Also note:** `ReposConfig::path()` may not exist as a public method. Add it if needed — it's just `home_dir + "/.codesearch/repos.json"`.

**Tests:**
- `config_reload_picks_up_new_alias`: construct `ServeState`, write `repos.json` with alias B after construction, call `aliases()` → B is in the list
- `config_reload_drops_removed_alias`: open alias X in DashMap (via `get_or_open_stores`), rewrite config without X → next `get_or_open_stores("X")` returns unknown-alias error and DashMap no longer contains X
- `config_reload_no_spurious_reload`: atomic counter incremented inside a test-only `ReposConfig::load` wrapper; two calls without mtime change → counter delta is 1
- `config_reload_tolerates_parse_error`: write garbage to `repos.json` → next lookup does not panic, warning is logged, old config still used

---

### 9. Per-repo file watchers in serve (Option A)

**Why.** Stdio-mode MCP already does live indexing via `IndexManager + FSW + git-HEAD watcher`. Serve mode currently doesn't — it lazy-opens stores but never refreshes them. A file edit in a served repo means stale search results until the user manually triggers a refresh (which conflicts with serve's write lock). This is the single biggest functional gap.

**Where.** `src/serve/mod.rs`. Three changes:

**(a) Extend `RepoState` to three variants:**

```rust
pub(crate) enum RepoState {
    /// Writable repo — full file watching + git HEAD watching active.
    Write {
        stores: Arc<SharedStores>,
        #[allow(dead_code)]  // kept alive for its spawned task
        index_manager: Arc<IndexManager>,
        cancel_token: CancellationToken,
    },
    /// Another process holds the write lock. Read-only access, no live updates.
    Readonly { stores: Arc<SharedStores> },
    /// Both write and readonly open failed.
    Conflicted,
}
```

Update the `Debug` impl and the fast-path match in `get_or_open_stores`:

```rust
RepoState::Write { stores, .. } | RepoState::Readonly { stores } => Ok(stores.clone()),
RepoState::Conflicted => Err(conflicted_msg(alias)),
```

**(b) After a successful `SharedStores::new` (write mode), spawn the watcher task:**

```rust
Ok(stores) => {
    let stores_arc = Arc::new(stores);
    info!("Opened repo '{}' in write mode", alias);

    // Try to create IndexManager; if it fails, still store Write but with a dummy
    // cancel_token — searches keep working, only live-update fails.
    let (index_manager_arc, cancel_token) = match IndexManager::new_without_refresh(&path, stores_arc.clone()).await {
        Ok(im) => {
            let im_arc = Arc::new(im);
            let token = CancellationToken::new();
            let project_path = path.clone();
            let db_path_clone = db_path.clone();
            let stores_for_task = stores_arc.clone();
            let im_for_task = im_arc.clone();
            let token_for_task = token.clone();

            tokio::spawn(async move {
                // Pre-start FSW so changes during initial refresh aren't lost
                if let Err(e) = im_for_task.start_watching().await {
                    tracing::warn!("⚠️ Could not pre-start FSW for '{}': {}", alias_clone, e);
                }

                // Initial incremental refresh
                if let Err(e) = IndexManager::perform_incremental_refresh_with_stores(
                    &project_path, &db_path_clone, &stores_for_task,
                ).await {
                    tracing::error!("❌ Initial refresh for '{}' failed: {}", alias_clone, e);
                }

                if token_for_task.is_cancelled() {
                    return;
                }

                // Main file watcher loop — runs until cancel_token fires
                if let Err(e) = im_for_task.start_file_watcher(token_for_task).await {
                    tracing::error!("❌ File watcher for '{}' stopped: {}", alias_clone, e);
                }
            });

            (im_arc, token)
        }
        Err(e) => {
            tracing::warn!("⚠️ IndexManager init failed for '{}': {} — searches work, live updates disabled", alias, e);
            // Dummy: construct an IndexManager placeholder or wrap in Arc<RwLock<Option<_>>>?
            // Simplest: require IndexManager, skip watcher on error. Use a cancelled token so any
            // future cleanup is a no-op.
            let token = CancellationToken::new();
            token.cancel();
            // Instead of Arc<IndexManager>, use Arc<Option<IndexManager>> or store None.
            // Pragmatic choice: fall through to Readonly on IndexManager failure.
            //
            // See "Failure policy" below — pick one approach and stick to it.
            todo!("pick failure policy")
        }
    };

    self.repos.insert(alias.to_string(), RepoState::Write {
        stores: stores_arc.clone(),
        index_manager: index_manager_arc,
        cancel_token,
    });
    return Ok(stores_arc);
}
```

**Failure policy (pick one).** When `IndexManager::new_without_refresh` fails:
- **Option A (recommended):** still store `RepoState::Write` with a dummy cancel_token; searches work, watcher is absent. Requires some way to model the missing IndexManager (e.g. `index_manager: Option<Arc<IndexManager>>`).
- **Option B:** fall back to `RepoState::Readonly` — searches work, no watcher. Simpler type but loses the distinction between "readonly by design" and "readonly due to watcher init failure".

Go with **Option A** — change `index_manager` to `Option<Arc<IndexManager>>` in `RepoState::Write`. Log the init failure clearly so users see it in the serve logs.

**(c) Readonly path stays simple** — just `RepoState::Readonly { stores }`, no watcher task.

**Teardown.** When `reload_if_changed` (item 8) removes an alias whose state is `RepoState::Write { cancel_token, .. }`, fire the token. The spawned task sees it via `token.is_cancelled()` or via the token parameter passed to `start_file_watcher`. Task exits within ~1s.

**Cost note.** N repos = N FSW handles + N git-HEAD watchers + N tokio tasks. ReadDirectoryChangesW on Windows scales fine to 20+ handles. No pooling needed.

**Tests:**
- `serve_watcher_picks_up_file_change`: spawn serve, open repo A (write mode), touch a file under A, wait for debounce (use a deterministic signal if `IndexManager` exposes one; otherwise `tokio::time::sleep(Duration::from_millis(500))` is acceptable for this test only, documented as such), assert new content appears in a subsequent `search(project="A", query=...)` call
- `serve_readonly_repo_has_no_watcher`: hold write lock externally (open SharedStores in another process or mock the lock), open repo in serve → state is `Readonly`, assert no tokio task spawned (observable via an atomic counter incremented in the spawned task — counter stays at 0 for this test)
- `serve_removed_alias_cancels_watcher`: open repo in write mode, remove from `repos.json` on disk, trigger reload via `reload_if_changed` (indirectly via a lookup), assert cancel_token was fired within 1 second (check `token.is_cancelled()` on a cloned handle)
- `serve_watcher_init_failure_keeps_searches_working`: simulate IndexManager::new_without_refresh failure (mock path doesn't exist, or corrupt `.git/`), assert `RepoState::Write` is still stored with `index_manager: None`, and a subsequent `get_or_open_stores` returns the stores Arc normally

---

### 10. `list_projects` — live lock status from `serve_state`

**Why.** Currently uses `is_database_locked(&db_path)` (a disk-check) regardless of whether serve is running. In serve mode, the real source of truth is `serve_state.repos` — so the reported status can drift from reality.

**Where.** `src/mcp/mod.rs`, inside the `list_projects` method (or whichever function handles `status(kind="projects")` after item 2).

When `self.serve_state` is `Some`:
1. Call `serve_state.reload_if_changed().ok()` first (cheap mtime check)
2. Iterate `serve_state.repos` (the DashMap) to derive per-alias lock_status:
   - `RepoState::Write { .. }` → `"write"`
   - `RepoState::Readonly { .. }` → `"readonly"`
   - `RepoState::Conflicted` → `"conflicted"`
3. For aliases registered in `config.repos` but NOT yet in the DashMap (never queried):
   - Fall back to `is_database_locked(&db_path)` → `"available"` (unlocked) or `"locked-externally"` (locked by another process)

When `self.serve_state` is `None` (stdio mode): keep existing behavior (disk check per repo).

**Test:**
- `list_projects_reports_lock_status_from_serve_state`: set up a ServeState with alias A as `RepoState::Write` and alias B as `RepoState::Readonly`, call `list_projects`, assert the response shows A as `"write"` and B as `"readonly"`
- `list_projects_falls_back_for_unopened_aliases`: alias C is in config but not yet opened in DashMap; response shows C as `"available"` (or `"locked-externally"` if the fixture puts an external lock there)

---

### 11. README update

**Why.** The current README documents `codesearch repos add/rm/list`, the deprecated aliases table, and has no mention of groups or the serve-mode live-update behavior you're about to add. Ship-breaking if left as-is.

**Where.** `README.md`.

**Remove:**
- All `codesearch repos add/rm/list` rows from the "Other Commands" table
- The entire "Repository Management" section that describes the `repos` commands
- The "Deprecated Aliases (still functional)" table under "MCP Tools"

**Rewrite "Repository Management" section as "Index & Project Management":**
```markdown
## Index & Project Management

`codesearch index` creates, refreshes, and manages indexes. Use one of these subcommands:

```bash
codesearch index add [PATH] [--alias NAME] [--global]
# Creates an index AND registers it in ~/.codesearch/repos.json
# --alias: assign a custom alias (auto-generated from dir name otherwise)
# --global: create a global index at ~/.codesearch.dbs/ (not auto-registered)

codesearch index rm  [PATH] [--keep-config]
# Removes the index AND unregisters it (symmetric with add)
# --keep-config: delete the DB only, preserve the repos.json entry

codesearch index list
# Shows all registered repos, their index state, and any groups
```

Re-indexing (`codesearch index` without add/rm) leaves the config alone.
```

**Add new "Groups" section:**
```markdown
## Groups

Groups let you search across multiple related repos in a single MCP call — useful for refactoring across a shared library and its consumers, or finding where a symbol is used across a whole platform.

Groups are created manually by you. AI agents don't create them — only you know which repos belong together.

```bash
codesearch groups add platform --aliases shared-lib service-a service-b
codesearch groups list
codesearch groups rm platform
```

From an AI agent session:
```
search(mode="semantic", group="platform", query="where is the auth token validated?")
```
fans out across all repos in the group and returns merged, alias-prefixed results (e.g. `shared-lib/src/auth.rs:42`).
```

**Update the "MCP Serve Mode" section:**
- Proxy auto-detect: stdio MCP probes `/health` at startup; if serve is running → proxy mode; if not → stdio mode; if version mismatch → hard error
- Dead session: if serve dies mid-session, all subsequent tool calls return a fixed error; client must reconnect after restarting serve
- **Live file watching:** each opened repo gets its own FSW + git-HEAD watcher; file edits and branch switches are picked up automatically — no manual re-index needed
- **Config reload:** `repos.json` changes are picked up on the next tool call, no serve restart required

**Update "MCP Tools" table to 5 primary rows** (no aliases table):

| Tool | Parameters | Description |
|---|---|---|
| `search` | `query`, `mode` ("semantic" default, "literal"), `limit`, `compact`, `filter_path`, `project`, `group` | Unified code search |
| `find` | `kind` ("definition" default, "usages", "imports", "dependents"), `symbol`, `limit`, `project`, `group` | Symbol navigation |
| `explore` | `kind` ("outline" default, "similar"), `target`, `limit`, `project` | File exploration |
| `get_chunk` | `chunk_id`, `context_lines`, `project` | Read chunk by ID |
| `status` | `kind` ("index" default, "projects") | Index + project info |

**Add troubleshooting rows:**
| Problem | Solution |
|---|---|
| `codesearch index` hangs or errors during a serve session | Serve holds the write lock. Let serve's built-in watcher pick up changes, or stop serve first |
| New repo added with `index add` not visible to running serve | No action needed — serve detects `repos.json` changes on the next tool call |
| Serve shows "database not found" for a registered repo | The `.codesearch.db/` was removed externally. Run `codesearch index add <path>` to recreate, or `codesearch index rm <path>` to clean up the config entry |
| Serve + MCP version mismatch | Stop serve, install matching binary on both endpoints, restart serve |

**Update "Other Commands" table** — remove the `repos` rows, add:
```
codesearch index add [PATH] [--alias NAME]   Create index AND register
codesearch index rm  [PATH] [--keep-config]  Remove index AND unregister
codesearch index list                          Show all registered repos + groups
codesearch groups add <n> --aliases A B...   Create/update a group
codesearch groups rm  <n>                    Remove a group
codesearch groups list                         List all groups
```

---

## Acceptance criteria

Everything below must hold before opening a PR against master:

- `cargo test --all` passes — all 286 existing tests plus the new ones from items 1-10
- `cargo clippy --all-targets -- -D warnings` passes
- `test_mcp_no_raw_stdout_calls` still passes (no `print!`/`println!` in `src/mcp/`)
- `test_instructions_max_50_lines` still passes after item 2 trims the deprecated aliases table
- Manual smoke test script below succeeds end-to-end

---

## Manual smoke test (put in the PR description)

```bash
# Setup: two repos + a group
mkdir /tmp/repo-a /tmp/repo-b && cd /tmp/repo-a && git init && echo "fn a() {}" > main.rs
cd /tmp/repo-b && git init && echo "fn b() {}" > main.rs

codesearch index add /tmp/repo-a                   # → "Registered as 'repo-a'"
codesearch index add /tmp/repo-b --alias bravo     # → "Registered as 'bravo'"
codesearch index list                              # → shows both, no groups

codesearch groups add pair --aliases repo-a bravo
codesearch groups list                             # → "pair → [repo-a, bravo]"

# Serve + MCP
codesearch serve --port 39725 &
# In another terminal, connect an MCP client (opencode or claude code):
#   status(kind="projects")                        → shows repo-a + bravo + pair
#   search(mode="semantic", project="repo-a", query="fn a")
#     → result path is "repo-a/main.rs" (alias-prefixed)
#   search(mode="semantic", group="pair", query="fn")
#     → results from both, alias-prefixed, no dup (alias, chunk_id)

# Hot add (serve keeps running)
mkdir /tmp/repo-c && cd /tmp/repo-c && git init && echo "fn c() {}" > main.rs
codesearch index add /tmp/repo-c
#   status(kind="projects")                        → now shows repo-c

# Hot rm (serve keeps running)
codesearch index rm /tmp/repo-a
#   search(project="repo-a", ...)                  → "Unknown alias" error
#   status(kind="projects")                        → pair contains only [bravo]

# Live file watching
echo "fn newthing() {}" >> /tmp/repo-b/main.rs
# Wait ~2 seconds
#   search(project="bravo", query="newthing")      → returns the new line

# Missing DB recovery
rm -rf /tmp/repo-b/.codesearch.db
#   search(project="bravo", ...)
#     → "Database not found ... Run `codesearch index add /tmp/repo-b` to recreate"
codesearch index add /tmp/repo-b
#   search(project="bravo", ...)                   → works (no serve restart)

# Version mismatch
# Build a binary with a different Cargo.toml version, run it as `mcp` against the serve:
#   → hard error: "serve at http://127.0.0.1:39725 reports vX, this binary is vY. To fix: ..."
```

---

## Out of scope

- Remote serve (non-localhost)
- HTTP authentication / OAuth
- Auto-start of serve from `mcp` (forking) — rejected
- Indexer-side dedup of stale chunks — separate future branch
- Non-AST file indexing (Markdown, YAML)
- Query expansion, import-graph persistence
- File-watcher on `repos.json` — the mtime-check in item 8 is sufficient
- `index rename` — edit `repos.json` manually or `rm` + `add --alias`
- Restoring `repos` subcommand as a deprecated CLI alias — clean removal only
- Restoring the deprecated MCP tool aliases — same
