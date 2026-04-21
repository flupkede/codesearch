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
- **A running serve doesn't notice new repos:** if the user runs `codesearch index add` (or edits `repos.json` directly) while serve is running, the new repo isn't queryable until serve is restarted. Likewise for removed repos — serve keeps them "open" until restart.
- **`index add` doesn't register the repo with serve:** creating an index in a new directory or worktree only creates the local `.codesearch.db/`; it does not write an entry to `~/.codesearch/repos.json`. The user has to run `codesearch repos add <path>` as a separate step, which is easy to forget and makes the new repo invisible to any running serve.

## Scope — what to implement in this phase

1. **Tool-handler routing.** Make every search/navigation tool resolve the correct stores via `ServeState` when `project` or `group` is provided.
2. **Cross-repo fan-out + merge.** For group queries, run the search per repo in parallel and merge results with rank-based RRF.
3. **Alias-prefixed paths in output.** All response paths in serve/proxy mode must be `{alias}/{relative_path}`.
4. **Validation errors.** `project`/`group` in stdio mode without serve → clear error message, not silent ignore.
5. **Acceptance tests.** Add the tests listed in AGENTS_multi_repo.md §Acceptance criteria that are still missing.
6. **Auto-register in `index add`.** When the user creates a new index in a directory (including a new worktree), automatically add an entry to `~/.codesearch/repos.json` if one doesn't already exist for that path.
7. **Config reload on demand in serve.** `ServeState` detects changes to `~/.codesearch/repos.json` via mtime comparison on every lookup, reloads transparently, and cleans up stores for removed aliases.

## Scope — what NOT to touch

- `src/serve/mod.rs`'s `run_serve()` function — it works as-is.
- `src/mcp/proxy.rs` — leave the proxy mechanics alone.
- `src/db_discovery/repos.rs` — ReposConfig is complete. **Exception:** you may add a small helper like `ReposConfig::load_if_changed_since(mtime)` if it cleans up the reload logic, but keep the public API stable.
- CLI subcommands — `serve`, `repos`, `groups` are wired. **Exception:** the `index add` handler needs the auto-register call (see §7).
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

### Auto-register on `index add`

**Problem.** Today, `codesearch index add <path>` (or its equivalent in `IndexCommands::Add`) creates the `.codesearch.db/` directory and populates it, but does not write an entry to `~/.codesearch/repos.json`. The user has to separately run `codesearch repos add <path>`, which most users will forget. A running serve then has no way to know about the new repo, even after reloading config — because the config literally doesn't contain it.

**Fix.** Hook into the `IndexCommands::Add` CLI handler (and any equivalent path that creates a fresh index, including the auto-create branch inside `run_mcp_server`). After the index is created successfully, load `ReposConfig`, check whether an entry already exists for the canonical path, and if not, call `ReposConfig::register(path)` and `save()`. Log the alias that was assigned so the user sees it.

Behaviour details:

- Canonicalize the path before comparing (reuse the logic in `ReposConfig::register`).
- If an entry already exists for the same canonical path (same path, different alias, or re-indexing of an existing repo), leave the config alone — do not re-register.
- If `repos.json` can't be written (permissions, disk full, etc.), log a warning but do not fail the index operation — indexing succeeded, the registration is a convenience layer.
- Print one line to stderr: `✅ Registered as '{alias}'. Use this as the `project` parameter in serve-mode queries.`
- Do not touch any existing auto-create branch in `run_mcp_server` that creates a fallback `.codesearch.db/` without user intent — that code path specifically handles the no-DB-no-create-flag case and shouldn't register without explicit user action. Only register when the user explicitly ran `index add`.

**Global-index case.** When `index add --global` is used, the index is created in a global location, not in the current directory. The `repos.json` entry should still use the project path (not the global DB location) so that `ServeState::get_or_open_stores` can locate the DB via the usual `project_path.join(DB_DIR_NAME)` convention. If the global-index layout stores the DB somewhere else, either skip auto-register for `--global` (document it clearly) or extend `ReposConfig` with an optional explicit `db_path` override per entry. Start with "skip + warn": *"Global indexes are not auto-registered. Use `codesearch repos add` manually or consider creating a local index instead."* This keeps the scope contained.

**Worktree case.** Each git worktree is a distinct project root (already handled correctly by the existing worktree detection in `find_git_root`). The auto-register produces a unique alias via `unique_alias_for_path`, so running `index add` in two worktrees of the same repo results in two distinct aliases (e.g. `codesearch` and `codesearch-2`). This is the desired behaviour — each worktree has its own index and should be addressable independently.

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

**Interaction with the `--register` CLI flag on serve.** When `codesearch serve --register /some/path` is used at startup, the existing code already loads config, calls `register`, and saves before `ServeState::new`. This means the initial mtime captured will be the post-registration mtime. Good — no spurious reload on first query.

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
- `index_add_auto_registers`: run `index add` programmatically (or via CLI in a test harness) in a temp directory; assert `~/.codesearch/repos.json` contains an entry for that path with a plausible alias; assert running `index add` again in the same directory does not create a duplicate entry.
- `index_add_global_skips_register`: run `index add --global` in a temp directory; assert `repos.json` is NOT modified; assert a user-visible warning was emitted.
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
9. Implement auto-register in `index add` (self-contained CLI change).
10. Implement `reload_if_changed` in `ServeState` and wire it into the lookup methods.
11. Write the acceptance tests.

Each step is a commit.

## Commit hygiene

- One logical change per commit.
- Conventional-commit style: `feat(mcp): add RepoContext resolver`, `feat(mcp): route file_outline via project param`, `feat(cli): auto-register repo on index add`, `feat(serve): reload repos config on mtime change`, `test(mcp): add cross-repo search integration test`, etc.
- Author: Filip Develter personal GitHub (`flupkede`).

## PR expectations

- Title: `feat(mcp): complete multi-repo routing, cross-repo fan-out, and live config reload`
- Target base: `main`.
- PR description must include:
  - A "Before/after" section: what works now that didn't before.
  - A manual-test script the reviewer can run: start serve, register 2 repos, call `status(kind="projects")`, call `search(mode="semantic", group="both", query="...")`, verify alias-prefixed results. **Also:** in a third terminal run `codesearch index add /tmp/new-repo`, then without restarting serve call `status(kind="projects")` again and verify the new repo appears.
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
