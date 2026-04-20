# AGENTS — Branch `feature/mcp-multi-repo`

> Scoped instructions for any coding agent (OpenCode, Claude Code) working on this branch.
> Self-contained: everything needed to implement the work is below.
> Parallel branches A, B, and `feature/mcp-navigation-extras` handle separate concerns — do not touch them here.

## Why this branch exists

codesearch currently runs as a per-project MCP server (one process per repo, stdio transport). This creates two hard problems:

1. **Claude Desktop has no project-scoped MCP config.** It loads from a single global `claude_desktop_config.json`. There is no way to give it a per-repo codesearch today.
2. **Cross-repo refactoring is impossible.** When renaming a symbol that spans multiple repos (shared lib + consumers), there is no single search surface that covers all of them.

This branch adds a `codesearch serve` HTTP/SSE mode and upgrades `codesearch mcp` (stdio) to auto-detect and proxy to a running serve instance. Existing single-repo behavior is fully preserved for users who never run `codesearch serve`.

## Architecture decisions (do not re-debate in implementation)

These were explicitly decided and must be followed as specified:

### Transport: Streamable HTTP (MCP spec 2025-03-26)
`codesearch serve` binds on `127.0.0.1:39725` (default, configurable via `--port` and `CODESEARCH_SERVE_PORT`). Uses rmcp's existing `transport-sse-server` / streamable-HTTP feature — no new HTTP framework needed beyond what rmcp provides.

### Auto-detect proxy in `codesearch mcp`
At stdio startup, `codesearch mcp` does a single HTTP GET `/health` on the configured port (200ms timeout). Response must be JSON containing `"codesearch-server": true` and a `version` field. Version mismatch (serve version ≠ mcp binary version) → hard error with clear message, do NOT proxy to a mismatched server.

- **Serve reachable:** switch to proxy mode. Open no local DB, take no locks. Forward all MCP tool calls as HTTP to the serve instance.
- **Serve not reachable (timeout / refused / wrong response):** fall through to stdio mode — open local DB exactly as today. Zero behavior change for users without a running serve.

### Failure handling in proxy mode
If the serve connection breaks mid-session (after startup health check passed):
- All subsequent tool calls return a fixed error response: *"codesearch serve is no longer reachable at {url}. This MCP session cannot recover. Restart the MCP client to reconnect, or restart `codesearch serve` first."*
- Do NOT attempt reconnect. Do NOT fall back to local DB (risk of write-lock conflict with a potentially still-running serve).
- Do NOT log to stdout (MCP protocol constraint — use tracing to stderr only).

### Lock rule — the single most important invariant
**At most one process may hold a write-lock on any `.codesearch.db/` at any time.**

- `codesearch serve` acquires write-locks lazily (on first query to that repo), not at startup.
- If a write-lock fails (another stdio-mode MCP already holds it): that repo is marked `conflicted` in serve's internal state. Queries for that repo return: *"Repo '{alias}' is currently locked by another codesearch process. Stop that process and restart serve, or use the standalone MCP for that repo."*
- Serve continues to work for all other repos. One conflicted repo does not take down the serve instance.
- `codesearch mcp` in stdio mode: uses existing `SharedStores::new_or_readonly` logic unchanged. No new behavior.
- Startup race (mcp and serve start simultaneously): locks are the source of truth. HTTP-detection is optimistic, not a guarantee. Accept suboptimal behavior (mcp stays local even though serve starts) — it resolves at next mcp restart. Never sacrifice lock safety for convenience.

### Project groups
`~/.codesearch/repos.json` schema (extend existing format):

```json
{
  "repos": {
    "codesearch": "/path/to/codesearch",
    "shared-lib": "/path/to/shared-lib",
    "service-a": "/path/to/service-a"
  },
  "groups": {
    "platform": ["shared-lib", "service-a"]
  }
}
```

- `repos`: map of alias → absolute path. Alias auto-generated from directory name on registration, user can rename.
- `groups`: named collections of aliases. No `"all"` keyword — must use explicit group or alias.
- Tool calls accept either `project: "alias"` or `group: "groupname"`. Not both simultaneously.
- Registration: `codesearch serve --register /path/to/repo` adds to `repos.json` (or auto-generates alias from dirname). `codesearch groups add <name> <alias1> <alias2>` creates a group.

### Cross-repo ranking
- Per-repo: fetch top `limit * 3` results via existing RRF fusion.
- Merge across repos: re-rank by RRF rank (not raw score — scores are not comparable across different LMDB instances).
- Path prefix in all output: `alias/relative/path.rs:line` format. Never expose full absolute paths in tool output.
- Aggregate result count: `limit` total across all repos, not `limit` per repo.

---

## File-by-file plan

### `~/.codesearch/repos.json` (schema only)
Extend the existing `repos.json` format. Add `groups` key. Keep backward-compatible: old files without `groups` load fine (treat as empty groups map).

Add `src/db_discovery/repos.rs` (or extend existing `db_discovery/mod.rs`):

```rust
#[derive(Serialize, Deserialize, Default)]
pub struct ReposConfig {
    pub repos: HashMap<String, PathBuf>,     // alias → path
    pub groups: HashMap<String, Vec<String>>, // group name → [alias]
}

impl ReposConfig {
    pub fn load() -> Result<Self> { ... }   // reads ~/.codesearch/repos.json
    pub fn save(&self) -> Result<()> { ... }
    pub fn register(&mut self, path: PathBuf) -> String { ... } // returns alias
    pub fn resolve(&self, project: &str) -> Option<PathBuf> { ... }
    pub fn resolve_group(&self, group: &str) -> Vec<(String, PathBuf)> { ... }
}
```

### `src/serve/mod.rs` (new module)

`codesearch serve` entry point. Responsibilities:

- Bind HTTP on configured address. Use rmcp's streamable-HTTP or SSE transport — check which is stable in the current rmcp version used by the project and use that.
- Expose `/health` GET endpoint returning `{"codesearch-server": true, "version": env!("CARGO_PKG_VERSION")}`.
- Hold a `DashMap<String, Arc<SharedStores>>` keyed by alias. Use the `dashmap` crate if already a dependency; if not, use `std::sync::RwLock<HashMap<...>>` to avoid adding deps.
- Lazy-open stores: on first query for alias X, attempt to open `SharedStores` for that path. If write-lock fails: mark as conflicted, return error response for that alias. If succeeds: insert into map and serve normally.
- Per-repo file watchers: start on first successful lock acquisition, stop on serve shutdown. Use existing `IndexManager` infrastructure.
- On SIGINT/SIGTERM (or Ctrl-C on Windows): graceful shutdown — stop all file watchers, close all stores, release all locks.
- CLI: `codesearch serve [--port N] [--register /path]`.

### `src/mcp/proxy.rs` (new file)

The stdio→HTTP proxy used when serve is detected.

```rust
pub struct McpProxy {
    base_url: String,
    http_client: reqwest::Client,
    dead: AtomicBool,
}

impl McpProxy {
    pub async fn check_health(base_url: &str) -> Result<bool> { ... }

    /// Forward a tool call to the serve instance.
    /// Sets dead=true on any connection error, after which all calls return the fixed error message.
    pub async fn forward(&self, tool: &str, params: serde_json::Value)
        -> Result<CallToolResult, McpError> { ... }
}
```

The fixed error message (dead=true): see "Failure handling in proxy mode" above. Exact wording matters for user clarity — use the specified text.

### `src/mcp/mod.rs` — startup logic

Add to `run_mcp_server()` before the existing DB-discovery logic:

```rust
// Step 0: check if a serve instance is reachable
let serve_url = format!("http://127.0.0.1:{}", serve_port);
if McpProxy::check_health(&serve_url).await? {
    // Proxy mode: do not open local DB, do not start file watcher
    let proxy = Arc::new(McpProxy::new(serve_url));
    let service = CodesearchProxyService::new(proxy);
    let server = service.serve(stdio()).await?;
    // ... wait for shutdown
    return Ok(());
}
// Step 1: existing stdio logic unchanged from here
```

Add `CodesearchProxyService` as a second MCP service implementation (same tool surface as `CodesearchService` but delegates to `McpProxy::forward`). Both services must expose identical tool schemas so MCP clients cannot distinguish which mode is active.

### `src/mcp/types.rs`

Add `project: Option<String>` and `group: Option<String>` to all search request types:
- `SemanticSearchRequest`
- `FindDefinitionRequest` (Branch A)
- `FindUsagesRequest` (Branch A)
- `LiteralSearchRequest` (Branch B)
- `FindReferencesRequest` (deprecated alias, Branch A)
- Navigation tools from `feature/mcp-navigation-extras` (stub only here)

Both `project` and `group` are `Option<String>`. If both provided: error. If neither: single-repo behavior (use the DB discovered at startup). Validation: if `project` or `group` is provided but serve is not active (stdio-mode): return clear error "project/group routing requires `codesearch serve` to be running."

### `src/mcp/mod.rs` — `list_projects` tool (rename of `find_databases`)

Rename `find_databases` → `list_projects`. Keep `find_databases` as deprecated alias (one release).

New response includes groups:

```rust
#[derive(Debug, Serialize)]
pub struct ListProjectsResponse {
    pub repos: Vec<RepoInfo>,
    pub groups: HashMap<String, Vec<String>>,
    pub serve_active: bool,
    pub serve_url: Option<String>,
    pub current_directory: String,
}

#[derive(Debug, Serialize)]
pub struct RepoInfo {
    pub alias: String,
    pub project_path: String,
    pub database_path: String,
    pub total_chunks: usize,
    pub total_files: usize,
    pub model: String,
    pub lock_status: String,  // "write", "readonly", "conflicted", "unknown"
}
```

### Cross-repo search implementation

In `CodesearchService` (local) and `CodesearchProxyService` (proxy), cross-repo search is only available in proxy mode (serve is running). In stdio mode, `project`/`group` params return an error.

In serve mode, for a group query:

```rust
async fn search_group(group: &str, request: SemanticSearchRequest) -> Vec<SearchResultItem> {
    let repos = config.resolve_group(group);
    let per_repo_limit = request.limit.unwrap_or(10) * 3;

    // Fan out — parallel per repo
    let futures: Vec<_> = repos.iter().map(|(alias, _)| {
        search_single_repo(alias, &request, per_repo_limit)
    }).collect();
    let results_by_repo = join_all(futures).await;

    // RRF merge across repos — rank-based, not score-based
    rrf_merge_cross_repo(results_by_repo, request.limit.unwrap_or(10))
}
```

Prefix all paths with alias in output: `format!("{}/{}", alias, relative_path)`.

---

## CLI additions

```
codesearch serve [OPTIONS]
    --port <PORT>          Bind port (default: 39725, env: CODESEARCH_SERVE_PORT)
    --register <PATH>      Register a repo and add to repos.json, then serve
    --quiet                Suppress startup banner

codesearch repos list      List all registered repos and groups
codesearch repos add <PATH> [--alias <NAME>]   Register a repo
codesearch repos remove <ALIAS>                Unregister
codesearch groups add <NAME> <ALIAS>...        Create/update a group
codesearch groups remove <NAME>                Remove a group
codesearch groups list                         List all groups
```

These subcommands only touch `~/.codesearch/repos.json` — they do not require serve to be running.

---

## Acceptance criteria

All must hold before PR merge:

- `cargo test --all` passes.
- `cargo clippy --all-targets -- -D warnings` passes.
- **Lock invariant test:** spawn two processes both attempting write-open on the same `.codesearch.db/`. Assert exactly one succeeds. Assert no data corruption after both exit. Run on Windows (NTFS/LMDB) explicitly.
- **Health endpoint test:** `codesearch serve` starts, GET `/health` returns `{"codesearch-server": true, "version": "..."}` within 500ms.
- **Version mismatch test:** mock a `/health` response with a different version string. Assert `codesearch mcp` emits a version-mismatch error and does NOT enter proxy mode.
- **Stdio fallback test:** no serve running, `codesearch mcp` starts, opens local DB, behaves as pre-this-branch. No regression.
- **Proxy-dead test:** serve starts, mcp connects in proxy mode, serve is killed mid-session. Next tool call returns the exact specified dead-session error message.
- **Conflicted-repo test:** stdio mcp holds write-lock on repo X. `codesearch serve` starts and tries to open repo X. Assert serve logs conflict, continues serving other repos, returns conflict error for repo X queries.
- **Cross-repo search test:** two indexed repos registered in a group. `semantic_search` with `group: "mygroup"` returns results from both, paths prefixed with alias.
- **`list_projects` test:** returns both `repos` and `groups`, `serve_active: true` when serve is running.
- `find_databases` still works as deprecated alias.
- Existing `test_mcp_no_raw_stdout_calls` passes.

---

## What NOT to implement here

The following are explicitly deferred:

- Import/dependency graph → `feature/mcp-navigation-extras`
- `file_outline`, `get_chunk`, `similar_chunks` → `feature/mcp-navigation-extras`
- Non-AST file indexing (Markdown, YAML, configs) → future branch
- Remote serve (non-localhost) → future branch; for now bind 127.0.0.1 only
- Authentication / OAuth on the HTTP endpoint → future branch; localhost-only is acceptable for v1
- Auto-start of serve from mcp (forking) → explicitly NOT wanted, do not implement
- `"all"` keyword for searching all repos — use explicit group instead

## Commit hygiene

- One logical change per commit.
- Conventional-commit style: `feat(serve): add HTTP serve mode`, `feat(mcp): add proxy auto-detect`, `feat(repos): add group config`, etc.
- Author: Filip Develter personal GitHub (`flupkede`). Verify `git config user.email` before first commit.

## PR expectations

- Title: `feat(mcp): add multi-repo serve mode with auto-detect proxy`
- Target base: `main` (ideally after Branches A and B have merged; rebasing on top of their changes is expected).
- PR description must include:
  - Architecture diagram (ASCII is fine) showing stdio-mode vs proxy-mode flow.
  - Lock invariant explanation.
  - How to set up Desktop + OpenCode for multi-repo use (step-by-step, 5 lines max).
  - Link to this AGENTS file.
- Draft PR acceptable while lock-invariant test is still failing on Windows.
