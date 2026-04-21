# AGENTS — Phase 2 for `feature/mcp-multi-repo`

> Scoped follow-up instructions for OpenCode / Claude Code. The original AGENTS_multi_repo.md laid the foundation; this file covers the remaining gap to make serve mode actually functional end-to-end.
>
> **Read first:** `AGENTS_multi_repo.md` (original spec) for context on architecture decisions, lock invariants, and the overall design. Everything below assumes that foundation.

## Current state (as of 2026-04-21)

Phase 1 built the infrastructure: `src/serve/mod.rs`, `src/mcp/proxy.rs`, `src/db_discovery/repos.rs` with groups, CLI subcommands (`serve`, `repos`, `groups`), and the consolidated tool surface (`search`, `find`, `explore`, `get_chunk`, `status`).

**What works:**
- `codesearch serve` binds and exposes `/health` + streamable HTTP at `/mcp`
- `codesearch mcp` detects running serve and enters proxy mode
- `McpProxy` handles connection lifecycle, version mismatch, dead-session errors
- `ServeState::get_or_open_stores(alias)` lazy-opens repos with write→readonly→conflicted fallback
- All tool request types accept `project: Option<String>` and `group: Option<String>`
- CLI: `codesearch repos add/list/rm`, `codesearch groups add/list/rm`

**What doesn't work:**
- Every tool call in serve mode fails with *"No index database found at serve://multi-repo"* because the tool handlers don't route via `project`/`group` to `ServeState` — they still use `self.db_path` / `self.shared_stores` which are placeholders in serve mode
- Group queries don't fan out across repos
- Output paths don't include the alias prefix
- `project`/`group` params are silently ignored in stdio mode instead of returning a clear error
- Key acceptance tests from AGENTS_multi_repo.md are not implemented
- **A running serve doesn't notice new or removed repos:** if the user runs `codesearch index add` / `index rm` (or edits `repos.json` directly) while serve is running, the change isn't reflected until serve is restarted.
- **`index add` doesn't register the repo with serve:** creating an index in a new directory or worktree only creates the local `.codesearch.db/`; it does not write an entry to `~/.codesearch/repos.json`. The user has to run `codesearch repos add <path>` as a separate step, which is easy to forget.
- **Having two parallel command surfaces is confusing:** `codesearch index add/rm` and `codesearch repos add/rm` do overlapping things from the user's perspective (both manage what repos are "known" to the system). This phase collapses them — `index` becomes the only entry point.

## Scope — what to implement in this phase

1. **Tool-handler routing.** Make every search/navigation tool resolve the correct stores via `ServeState` when `project` or `group` is provided.
2. **Cross-repo fan-out + merge.** For group queries, run the search per repo in parallel and merge results with rank-based RRF.
3. **Alias-prefixed paths in output.** All response paths in serve/proxy mode must be `{alias}/{relative_path}`.
4. **Validation errors.** `project`/`group` in stdio mode without serve → clear error message, not silent ignore.
5. **Acceptance tests.** Add the tests listed in AGENTS_multi_repo.md §Acceptance criteria that are still missing.
6. **CLI consolidation: `index` is the single entry point.** Remove the `repos` top-level subcommand. `index add` registers, `index rm` unregisters, `index list` shows all registered repos + their index state. Groups keep their own subcommand (`groups add/list/rm`) because they operate on aliases, not paths.
7. **Auto-register / auto-unregister symmetry.** `index add` writes an entry to `~/.codesearch/repos.json`; `index rm` removes the entry (and from any groups that contained it). This falls out naturally from §6.
8. **Config reload on demand in serve.** `ServeState` detects changes to `~/.codesearch/repos.json` via mtime comparison on every lookup, reloads transparently, and cleans up stores for removed aliases.

## Scope — what NOT to touch

- `src/serve/mod.rs`'s `run_serve()` function — it works as-is.
- `src/mcp/proxy.rs` — leave the proxy mechanics alone.
- `src/db_discovery/repos.rs` — `ReposConfig` is complete. **Exception:** you may add a small helper like `ReposConfig::load_if_changed_since(mtime)` if it cleans up the reload logic, but keep the public API stable.
- `groups` subcommand — stays as-is. Groups operate on aliases; they're orthogonal to index management.
- The existing single-repo behavior of stdio-mode `codesearch mcp` — must stay unchanged when no serve is running.
- The consolidated tool surface (`search`/`find`/`explore`/`get_chunk`/`status`) and deprecated aliases — don't re-design.
- HTTP transport, authentication, non-localhost binding — deferred to future work.

## Key design decision — read before coding

### Routing resolution pattern

In every tool handler, replace this pattern:

```rust
async fn some_tool(&self, ...) -> Result<CallToolResult, McpError> {
    if let Err(e) = self.ensure_database_exists() { ... }
    let result = self.with_vector_store_read(|store| { ... }).await;
    ...
}
```

With a resolver that returns the right stores regardless of mode:

```rust
async fn some_tool(&self, ...) -> Result<CallToolResult, McpError> {
    let ctx = match self.resolve_project_context(request.project.as_deref(), request.group.as_deref()) {
        Ok(c) => c,
        Err(e) => return Ok(CallToolResult::success(vec![Content::text(e)])),
    };
    // ctx exposes: project_path, db_path, shared_stores (Arc), alias (Option<String>)
    let result = ctx.with_vector_store_read(|store| { ... }).await;
    ...
}
```

Add a helper on `CodesearchService`:

```rust
/// Projects the service into one or more repo contexts based on request params.
///
/// Returns a Vec<RepoContext> because group queries span multiple repos.
/// For single-repo queries, the Vec has length 1.
///
/// Modes:
/// - stdio mode (self.shared_stores is Some, self.serve_state is None):
///   - if project/group is Some → return Err("project/group routing requires `codesearch serve` to be running")
///   - if both are None → return Ok(vec![ctx_from_self])
/// - serve mode (self.serve_state is Some):
///   - if group is Some → resolve_group(group) → one RepoContext per alias
///   - if project is Some → resolve(project) → one RepoContext
///   - if both None → return Err("this serve instance requires `project` or `group` parameter")
///   - if both Some → return Err("pass either `project` or `group`, not both")
/// - proxy mode (self.proxy is Some): never reached — handled by forward()
fn resolve_contexts(
    &self,
    project: Option<&str>,
    group: Option<&str>,
) -> Result<Vec<RepoContext>, String>
```

Where `RepoContext` bundles what a handler needs:

```rust
pub(crate) struct RepoContext {
    /// alias (Some in serve mode, None in stdio mode)
    pub alias: Option<String>,
    pub project_path: PathBuf,
    pub db_path: PathBuf,
    pub shared_stores: Arc<SharedStores>,
    pub dimensions: usize,
    pub model_type: ModelType,
}
```

The existing helpers `with_vector_store_read` / `with_fts_store_read` should be moved onto `RepoContext` (or take `&RepoContext`) so every handler uses the same read helpers regardless of mode.

### Proxy forwarding

In proxy mode, every tool handler should forward the entire request to the serve instance. Don't try to split into per-tool forward logic — use a single generic `forward(tool_name, params_json)` that matches the JSON-RPC tool-call shape, then deserialize the response:

```rust
async fn some_tool(&self, Parameters(request): Parameters<SomeRequest>) -> Result<CallToolResult, McpError> {
    if let Some(ref proxy) = self.proxy {
        let params = serde_json::to_value(&request).unwrap_or(serde_json::Value::Null);
        return proxy.forward("some_tool", Some(params)).await
            .map_err(|e| McpError::internal_error(e.message.into_owned(), None));
    }
    // normal stdio/serve mode logic
}
```

This is the pattern `list_projects` already follows. Replicate it consistently across all tool handlers.

## File-by-file plan

### `src/mcp/mod.rs` — add `RepoContext` + `resolve_contexts`

Near the top of the file, define `RepoContext`. Build it from:
- stdio mode: `self.project_path`, `self.db_path`, `self.shared_stores.clone().unwrap()`, `self.dimensions`, `self.model_type`. alias = `None`.
- serve mode: call `serve_state.get_or_open_stores(alias)` → `Arc<SharedStores>`. project_path comes from `serve_state.resolve_alias(alias)`. db_path = project_path.join(DB_DIR_NAME). dimensions + model_type from `read_model_metadata(&db_path)`.

Move the existing `with_vector_store_read` and `with_fts_store_read` methods to `impl RepoContext`, or create thin wrappers on `RepoContext` that delegate. Whichever is cleaner — minor refactor, not a rewrite.

### `src/mcp/mod.rs` — update every tool handler

Handlers to update (the list is exhaustive):

- `semantic_search` (delegate from `search`)
- `literal_search` (delegate from `search`)
- `find_definition`
- `find_usages` / `find_usages_impl`
- `find_references` (alias for find_usages)
- `find_imports`
- `find_dependents`
- `similar_chunks`
- `file_outline`
- `get_chunk`
- `index_status` (delegate from `status`)
- `list_projects` — already has partial proxy logic; extend to iterate serve_state.aliases() when in serve mode
- `find_databases` (deprecated alias)

For each handler:

1. If `self.proxy.is_some()` → forward and return. (Generic proxy forwarding described above.)
2. Call `self.resolve_contexts(request.project.as_deref(), request.group.as_deref())`.
3. If single context → existing logic adapted to use `RepoContext` helpers.
4. If multiple contexts (group) → fan out, merge results (see next section).

Delete the `let _project = request.project.as_deref();` no-op lines everywhere.

### Cross-repo fan-out for group queries

Only applies to `semantic_search`, `literal_search`, `find_definition`, `find_usages`, `find_dependents`. `find_imports`, `file_outline`, `get_chunk`, `similar_chunks` are inherently single-repo — if called with `group`, return an error: *"Tool 'X' operates on a single repo. Use `project` instead of `group`."*

Fan-out pattern:

```rust
async fn search_across_contexts(
    contexts: Vec<RepoContext>,
    request: &SemanticSearchRequest,
) -> Vec<SearchResultItem> {
    let per_repo_limit = request.limit.unwrap_or(10) * 3;

    // 1. Fan out in parallel
    let futures: Vec<_> = contexts.iter().map(|ctx| {
        single_repo_semantic_search(ctx, request, per_repo_limit)
    }).collect();
    let per_repo_results: Vec<(String, Vec<SearchResultItem>)> =
        futures::future::join_all(futures).await
            .into_iter()
            .zip(contexts.iter())
            .map(|(r, ctx)| (ctx.alias.clone().unwrap_or_default(), r.unwrap_or_default()))
            .collect();

    // 2. Prefix paths with alias
    let prefixed: Vec<(String, Vec<SearchResultItem>)> = per_repo_results.into_iter()
        .map(|(alias, results)| {
            let mapped = results.into_iter().map(|mut item| {
                item.path = format!("{}/{}", alias, strip_project_root(&item.path, &project_root));
                item
            }).collect();
            (alias, mapped)
        })
        .collect();

    // 3. Rank-based RRF merge (NOT score-based — scores aren't comparable across indexes)
    rrf_merge_by_rank(prefixed, request.limit.unwrap_or(10))
}
```

Add `rrf_merge_by_rank` as a pure helper. Formula: for each result at rank `r` in repo `i`, contribution score = `1.0 / (k + r)` with `k = 60` (standard). Sum contributions across repos per chunk-id. Sort descending, take top N.

**Important:** chunk_ids are not globally unique across repos. Dedup-key for RRF merge must be `(alias, chunk_id)`, not `chunk_id` alone. Keep `chunk_id` in the response — in cross-repo output, chunk_id is only meaningful when paired with the alias.

### Path prefix in single-repo serve mode too

Even for single-project calls in serve mode, prefix paths with the alias. Reasoning: the LLM should always see `myrepo/src/file.rs:42` so it knows which repo to navigate. This is consistent whether the user passed `project` or `group`. In stdio mode (no serve, no alias), leave paths as they are today.

Helper:

```rust
fn prefix_path_with_alias(path: &str, alias: Option<&str>, project_root: &str) -> String {
    let relative = path.strip_prefix(project_root).unwrap_or(path).trim_start_matches('/');
    match alias {
        Some(a) => format!("{}/{}", a, relative),
        None => path.to_string(),
    }
}
```

### Validation errors

`resolve_contexts` already returns clear error messages per the design decision above. Make sure those messages are surfaced to the LLM via `CallToolResult::success(vec![Content::text(err)])` — don't swallow them.

One extra case: in stdio mode when user passed `project="foo"` and it happens to match `self.project_path`'s directory name, don't be clever — still return the error. Stdio mode cannot route; it's a clean contract.

### `list_projects` in serve mode

Currently loads from disk config only. In serve mode, augment with live state from `serve_state`:

- For each alias from `serve_state.aliases()`:
  - If `serve_state.repos` has `Open{stores}` → lock_status = `"write"` (or `"readonly"` if that's tracked)
  - If `Conflicted` → lock_status = `"conflicted"`
  - Otherwise (not yet opened) → check on-disk lock via `is_database_locked()` (existing function)

Keep the proxy-forward branch at the top as already implemented.

### CLI consolidation: fold `repos` into `index`

**Goal.** Remove the `repos` top-level subcommand entirely. `index` becomes the single user-facing command for everything related to what repos the system knows about.

**Final CLI surface:**

```
codesearch index add <path> [--alias <name>] [--global]   Create index AND register in repos.json
codesearch index rm <path> [--keep-config]                Remove index AND unregister from repos.json
codesearch index list                                      Show all registered repos + their index state
codesearch groups add <name> <alias>...                   Create/update a group (unchanged)
codesearch groups rm <name>                                Remove a group (unchanged)
codesearch groups list                                     List groups (unchanged)
```

No `codesearch repos …` commands. They're gone.

**Behaviour of each `index` subcommand:**

- **`index add <path>`**
  - Canonicalize path.
  - Create the `.codesearch.db/` under that path if it doesn't exist. If the DB already exists (user is re-registering an existing index): skip creation, emit `ℹ️ Index already exists at {path}; reusing.`
  - Load `ReposConfig`. If an entry already exists for this canonical path: skip register, print `ℹ️ Already registered as '{existing_alias}'.` — do not error.
  - Otherwise: register with `register_with_alias(path, --alias)`. If `--alias` collides with an existing alias for a different path: error clearly (let `register_with_alias` handle this, it already does).
  - `--global`: build a global index. Do NOT auto-register — global indexes live in a shared location and don't map cleanly to per-project aliases. Emit: `⚠️ Global indexes are not auto-registered. Use 'codesearch index add <path>' without --global if you want serve to discover it.`
  - Print one-line success: `✅ Indexed {path} as '{alias}'. Use 'codesearch search --project {alias}' or rely on serve auto-discovery.`

- **`index rm <path>`**
  - Canonicalize path.
  - Look up the alias for this path via `ReposConfig::alias_for_path`. If none: the repo wasn't registered — proceed with DB deletion only, no config write.
  - If `--keep-config` is NOT set (default): remove the alias from `repos.json` via `unregister_path(&path)`. This also strips the alias from any groups that contained it, and drops any group that becomes empty (existing `unregister_alias` behaviour).
  - Delete the `.codesearch.db/` directory. Handle "directory in use" errors on Windows gracefully: emit `⚠️ Database files may be locked by a running codesearch process. Stop it and retry.`
  - `--keep-config`: leaves the `repos.json` entry intact. Useful for rare cases (scripted re-indexing). Print `ℹ️ Config entry preserved.`
  - On any failure: do not leave the system half-registered. If DB deletion fails, the config entry stays (reversible state). If config write fails, DB deletion still happens — emit a warning that the user should clean up `repos.json` manually.

- **`index list`**
  - Read `ReposConfig`.
  - For each alias, report: alias, project path, DB exists (yes/no), total chunks, total files, model, and — if serve is running on the default port — live lock_status from a quick HTTP query to the running serve (best effort; if serve isn't reachable, skip this column).
  - Pretty-print as a table to stdout. Also support `--json` for scripting.
  - Include groups at the end: one line per group showing `group_name → [alias1, alias2, …]`.
  - Keep the existing "current directory index status" idea as a separate hint at the bottom: "Current directory: {path} → {alias or 'not registered'}".

**Code changes:**

In `src/cli/mod.rs`:

- Remove `ReposCommands` enum and the `Repos { command: ReposCommands }` variant from the top-level `Commands` enum.
- Update `IndexCommands::Add` to include an `--alias` flag and keep `--global`.
- Update `IndexCommands::Remove` to include a `--keep-config` flag.
- `IndexCommands::List` stays but the handler now reads `ReposConfig` (not just the current directory).

In the corresponding CLI handler module (wherever `Commands::Index { … }` is dispatched):

- `Add`: call existing indexer AND `ReposConfig::register_with_alias` (new path — already exists in `ReposConfig`). Save.
- `Remove`: call `ReposConfig::unregister_path` unless `--keep-config`. Save. Then delete the DB directory.
- `List`: read `ReposConfig`, enumerate, format as table.

In tests, update any call-sites that used `Commands::Repos(…)` to use `Commands::Index(…)`.

**Do not** keep `repos` as a deprecated alias. This project is pre-1.0 and `repos` was only added in phase 1 of this same multi-repo work — it has never shipped. A clean removal is the right call.

### Config reload on demand in serve

**Problem.** `ServeState` loads `ReposConfig` once at startup and caches it. When the user adds, removes, or renames a repo (via CLI or by editing `repos.json` directly), the running serve keeps using the stale config. Newly-registered aliases return "unknown alias"; removed aliases continue to occupy memory and hold locks.

**Fix.** Reload lazily, gated by mtime comparison. This avoids polling and runs only when serve would otherwise fail or act on stale data.

**Implementation.**

Add fields to `ServeState`:

```rust
pub(crate) struct ServeState {
    repos: DashMap<String, RepoState>,
    config: RwLock<ReposConfig>,
    config_mtime: RwLock<Option<SystemTime>>,
    dimensions_cache: DashMap<String, usize>,
}
```

Change `ServeState::new` to capture the initial mtime of `repos.json` (via `config_path()` + `fs::metadata`). If the file doesn't exist, store `None`.

Add a private helper:

```rust
/// Reload config if `repos.json` has changed on disk since last read.
/// Cheap: one `stat` call per invocation. Safe to call on every lookup.
///
/// On reload, tears down stores for aliases that are no longer in the config.
fn reload_if_changed(&self) -> Result<()>
```

Implementation sketch:

1. `stat` the config path. If it doesn't exist and we previously had no mtime → no-op.
2. Compare current mtime to `config_mtime`. If equal → no-op. If different (or now exists where it didn't) → proceed.
3. Load new config via `ReposConfig::load()`. On parse error, log + keep old config, update mtime anyway (to avoid retrying every call).
4. Compute removed aliases: `old.repos.keys() - new.repos.keys()`.
5. For each removed alias, remove its entry from `self.repos`. When the entry was `Open{stores}`, the `Arc<SharedStores>` drop chain handles closing the LMDB env and stopping any FSW the stores own. If `ServeState` owns separate per-repo file watchers (currently it doesn't — file watchers are inside IndexManager, which isn't used in serve mode yet), stop them here explicitly.
6. Replace `self.config` with the new one. Update `self.config_mtime`.
7. Do NOT pre-open newly-added aliases. Keep the existing lazy-open behaviour — they'll be opened on first query.

Call `reload_if_changed` at the start of:
- `get_or_open_stores` (before the DashMap fast-path check).
- `aliases()` (so `list_projects` sees fresh config).
- `resolve_alias` if it's used outside `get_or_open_stores`.

**Concurrency.** Use `RwLock<ReposConfig>` because reads are frequent (every tool call) and writes are rare (config actually changed). The reload itself takes a write lock briefly. Avoid holding the write lock while dropping stores — collect the "to remove" list under the write lock, then release and do the `self.repos.remove()` calls afterwards.

**Race with concurrent tool calls.** If a tool call is mid-execution on alias X using a `stores: Arc<SharedStores>` clone, and another request triggers reload-which-removes-X, the in-flight call finishes cleanly because it holds its own `Arc`. After it drops, refcount hits zero, LMDB env closes. This is correct behaviour.

**Do not reload from a file watcher.** An explicit watcher on `repos.json` adds async plumbing and race conditions. On-demand mtime-check is simpler and sufficient for human-editing frequencies. If future performance shows this being hot, we can add debouncing.

**Interaction with `codesearch serve --register`.** The `--register` flag on `serve` stays — it's convenient for one-shot setup. It does the same thing as `index add` + `serve`. When used, the existing code already loads config, calls `register`, and saves before `ServeState::new`, so the initial mtime captured will be the post-registration mtime. No spurious reload on first query.

## Acceptance criteria

All must hold before PR merge:

- `cargo test --all` passes.
- `cargo clippy --all-targets -- -D warnings` passes.

**New tests required:**

- `resolve_contexts_stdio_rejects_project`: stdio-mode service + `project=Some("foo")` → Err.
- `resolve_contexts_serve_rejects_missing_project`: serve-mode service + both None → Err.
- `resolve_contexts_serve_rejects_both`: serve-mode + both Some → Err.
- `resolve_contexts_serve_group_fans_out`: serve-mode + `group="mygroup"` with 2 repos → returns 2 contexts.
- `rrf_merge_by_rank_pure`: unit test of the merge helper with deterministic input.
- `path_prefix_with_alias`: helper test for prefix behavior.
- `health_endpoint_live`: integration test — spawn `run_serve` on an ephemeral port, GET `/health`, assert JSON shape and version match.
- `version_mismatch_errors`: mock `/health` returning different version, assert `check_health` returns `Err`.
- `stdio_fallback_when_no_serve`: no serve running, `codesearch mcp` init goes to stdio path. Smoke test via `McpProxy::check_health(nonexistent_url).await → Ok(false)`.
- `lock_invariant_windows`: spawn two processes attempting write-open on the same `.codesearch.db/`. Assert exactly one succeeds. Must run on Windows. Mark `#[cfg(target_os = "windows")]` if needed.
- `conflicted_repo_isolated`: stdio mcp holds write-lock on repo X; spawn `run_serve` that tries to open repo X; assert serve marks X as Conflicted and other repos still work.
- `cross_repo_search_group`: fixture with two indexed repos; semantic_search with `group="both"` returns results from both, paths alias-prefixed, no duplicate `(alias, chunk_id)` pairs in output.
- `single_repo_tools_reject_group`: `file_outline` / `get_chunk` / `similar_chunks` / `find_imports` with `group=Some(...)` → error about single-repo tool.
- `cli_no_repos_subcommand`: assert `codesearch repos --help` returns a CLI error (subcommand removed). Assert `codesearch index add --help` shows the `--alias` and `--global` flags. Assert `codesearch index rm --help` shows the `--keep-config` flag.
- `index_add_auto_registers`: run `index add` programmatically in a temp directory; assert `repos.json` contains an entry for that path with a plausible alias; assert a second `index add` in the same directory does not create a duplicate entry and prints `Already registered as '{alias}'`.
- `index_add_with_explicit_alias`: `index add /tmp/x --alias myrepo` → `repos.json` has entry with alias `myrepo`.
- `index_add_alias_collision`: `repos.json` already has alias `foo` pointing to `/tmp/a`; `index add /tmp/b --alias foo` → error, no write.
- `index_add_global_skips_register`: `index add --global` in a temp directory; assert `repos.json` is NOT modified; assert the `--global` warning was emitted.
- `index_rm_auto_unregisters`: set up a registered repo with alias `x` that is also a member of group `g`; run `index rm /path`; assert alias `x` is gone from `repos.json`; assert group `g` no longer contains `x`; if `x` was the only member of `g`, assert `g` is also removed.
- `index_rm_preserves_config_with_flag`: run `index rm /path --keep-config`; assert DB is deleted but `repos.json` entry remains.
- `index_rm_unregistered_path_ok`: run `index rm /path` on a path that was never registered; assert no error, DB is still deleted.
- `index_list_shows_registered_repos`: register two repos + one group; run `index list`; assert all appear in output.
- `config_reload_picks_up_new_alias`: start `ServeState` with config containing repo A. Without restarting, append repo B to `repos.json` and update mtime. Call `serve_state.aliases()`; assert it includes B. Call `get_or_open_stores("B")`; assert success.
- `config_reload_drops_removed_alias`: start `ServeState` with repos A and B. Pre-open B. Rewrite `repos.json` with only A. Call `get_or_open_stores("B")`; assert error "Unknown alias". Assert the entry for B has been removed from `self.repos`.
- `config_reload_no_spurious_reload`: call `get_or_open_stores` twice in a row without touching `repos.json`; verify (via a counter or tracing assertion) that `ReposConfig::load` was called at most once.

Existing tests must all still pass, including `test_mcp_no_raw_stdout_calls`.

## Implementation order

Suggested order to minimize rework:

1. Define `RepoContext` + `resolve_contexts` (pure helper, easy to unit-test).
2. Add single-repo routing to one handler end-to-end (pick `index_status` — simplest). Run manually: `codesearch serve --register .` + `codesearch mcp` + call `status`. Verify the end-to-end flow works.
3. Propagate the pattern to the remaining single-repo handlers.
4. Add `rrf_merge_by_rank` + fan-out logic.
5. Wire group-capable handlers.
6. Add single-repo-tool rejection for `group` param.
7. Add path-prefix helper + apply in all handlers.
8. Extend `list_projects` serve-mode augmentation.
9. **CLI consolidation:** remove `ReposCommands`, add `--alias` to `IndexCommands::Add`, add `--keep-config` to `IndexCommands::Remove`, implement auto-register in Add handler, auto-unregister in Remove handler, rewrite List handler to read `ReposConfig`. One commit per change.
10. Implement `reload_if_changed` in `ServeState` and wire it into the lookup methods.
11. Write the acceptance tests.

Each step is a commit.

## Commit hygiene

- One logical change per commit.
- Conventional-commit style: `feat(mcp): add RepoContext resolver`, `feat(mcp): route file_outline via project param`, `refactor(cli): remove repos subcommand, fold into index`, `feat(cli): auto-register repo on index add`, `feat(cli): auto-unregister repo on index rm`, `feat(serve): reload repos config on mtime change`, `test(mcp): add cross-repo search integration test`, etc.
- Author: Filip Develter personal GitHub (`flupkede`).

## PR expectations

- Title: `feat(mcp): complete multi-repo routing, unified index CLI, and live config reload`
- Target base: `main`.
- PR description must include:
  - A "Before/after" section: what works now that didn't before. Call out the CLI consolidation prominently — `codesearch repos …` is gone.
  - A manual-test script the reviewer can run:
    1. `codesearch index add /tmp/repo-a` — assert success + registration message
    2. `codesearch index add /tmp/repo-b --alias bravo` — assert custom alias worked
    3. `codesearch index list` — assert both appear with correct aliases
    4. `codesearch groups add pair alpha bravo` (alpha was auto-generated from repo-a)
    5. `codesearch serve --port 39725 &` — start serve
    6. From MCP client: call `status(kind="projects")` — assert both repos + group visible
    7. Call `search(mode="semantic", group="pair", query="...")` — assert alias-prefixed results
    8. In a third terminal: `codesearch index add /tmp/repo-c` — without restarting serve
    9. Call `status(kind="projects")` again — assert repo-c appears
    10. `codesearch index rm /tmp/repo-a` — without restarting serve
    11. Call `search(..., project="alpha", ...)` — assert "Unknown alias" error
    12. Call `status(kind="projects")` — assert alpha is gone and group "pair" now contains only `bravo` (or was dropped if bravo was the only survivor)
  - Link to this AGENTS file and `AGENTS_multi_repo.md`.

## Deliberately out of scope

- Remote serve (non-localhost) — localhost-only stays for now.
- HTTP authentication / OAuth.
- Auto-start of serve from mcp (forking) — explicitly rejected in earlier discussion.
- Indexer-side dedup of stale chunks (this is a separate future branch).
- Tools-consolidation tweaks — don't redesign `search`/`find`/`explore`.
- Non-AST file indexing (Markdown, YAML, configs).
- Import-graph persistence as a separate index structure.
- Query expansion.
- File-watcher-based config reload — on-demand mtime-check is sufficient for v1.
- Auto-register for `--global` indexes — skip + warn is the v1 behaviour.
- Keeping `repos` as a deprecated CLI alias — clean removal instead.
- An `index rename` command — users can edit `repos.json` directly or `index rm` + `index add --alias`. Low enough priority to defer.
