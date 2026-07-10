# codesearch

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org/)
[![MCP server](https://img.shields.io/badge/MCP-server-8a2be2.svg)](https://modelcontextprotocol.io/)
[![GitHub release](https://img.shields.io/github/v/release/flupkede/codesearch?include_prereleases)](https://github.com/flupkede/codesearch/releases)
[![GitHub stars](https://img.shields.io/github/stars/flupkede/codesearch?style=social)](https://github.com/flupkede/codesearch/stargazers)

**Multi-repo semantic code search for AI agents — a Rust MCP server with vector + BM25 hybrid retrieval, symbol navigation, and cross-repository orchestration. Fully local, fully offline, no GPU, no Docker.**

codesearch gives AI agents (OpenCode, Claude Code, Cursor, and any MCP client) deep codebase understanding through 5 unified MCP tools. Index once, search semantically across multiple repositories simultaneously.

## Why codesearch?

- **Multi-repo serve mode**: Fan-out queries across repository groups with cross-repo RRF ranking
- **Hybrid retrieval**: Vector embeddings + BM25 full-text search fused with Reciprocal Rank Fusion
- **Symbol navigation**: Jump to definitions, find usages, trace imports and dependents — in the same tool
- **AST-aware chunking**: Tree-sitter parsing for 16 languages — chunks align to functions/classes (and Markdown sections), not arbitrary line ranges
- **Token-efficient**: Returns metadata by default; agents fetch full code only when needed via `get_chunk`
- **Lightweight footprint**: Hundreds of MB on disk, runs on CPU only, no runtime model downloads (works behind enterprise proxies)
- **Zero config for single repos**: `codesearch index && codesearch mcp` — done

## How does this compare?

The MCP code-search ecosystem grew rapidly in late 2025 / early 2026 and many projects share the same baseline stack (Rust + tree-sitter + BM25 + embeddings + MCP). codesearch's deliberate focus is:

| Focus area | codesearch | Typical alternative |
|---|---|---|
| Repository scope | Multi-repo serve with cross-repo RRF | Usually single repo at a time |
| Footprint | ~hundreds of MB, CPU-only, no Docker | GB-scale, GPU, Docker, or cloud |
| Enterprise / offline | No runtime fetches; static binary | Often pulls models at first run |
| Symbol navigation | `find` (def/usages/imports/dependents) co-located with semantic search | Often a separate code-graph tool |
| Token cost per call | `compact=true` by default; chunks fetched on demand | Frequently dumps full snippets |

codesearch is intentionally narrower than full code-graph or knowledge-graph tools — it picks "lightweight, multi-repo, MCP-native, fully offline" and stays on that lane.

## Architecture

```mermaid
graph TB
    Agent[AI Agent / MCP Client] -->|MCP stdio or HTTP| Router{MCP Router}

    Router --> Search[search tool]
    Router --> Find[find tool]
    Router --> Explore[explore tool]
    Router --> GetChunk[get_chunk tool]
    Router --> FindImpact[find_impact tool]
    Router --> Status[status tool]

    Search -->|mode=semantic| Semantic[Vector ANN + BM25 + RRF Fusion]
    Search -->|mode=literal| Literal[Tantivy FTS / Regex]

    Find -->|definition/usages| SymbolIndex[Symbol Index]
    Find -->|imports/dependents| DepGraph[Dependency Graph]

    Explore -->|outline| TreeSitter[Tree-sitter AST]
    Explore -->|similar| Semantic

    Semantic --> Arroy[arroy ANN vectors]
    Semantic --> Tantivy[Tantivy BM25]
    Arroy --> LMDB[(LMDB)]
    Tantivy --> TantivyIdx[(Tantivy Index)]

    GetChunk --> LMDB

    FindImpact -->|C# symbols| CSharpHelper[scip-csharp helper]
    CSharpHelper -->|SCIP index| ScipLMDB[(LMDB scip_symbols)]

    subgraph "Serve Mode (multi-repo + federation)"
        ServeRouter[HTTP Router] -->|project/group routing| Repo1[Repo A]
        ServeRouter --> Repo2[Repo B]
        ServeRouter --> RepoN[Repo N]
        ServeRouter -->|"@peer fan-out · TLS"| CloudPeer["Cloud serve peer<br/>results merged via RRF"]
    end

    Router -->|client mode| ServeRouter
```

## Quick Start

### Install

Download pre-built binaries from [Releases](https://github.com/flupkede/codesearch/releases):

| Platform | Download |
|----------|----------|
| Windows x86_64 | `codesearch-windows-x86_64.zip` |
| Windows x86_64 + C# | `codesearch-windows-x86_64-with-csharp.zip` |
| Linux x86_64 | `codesearch-linux-x86_64.tar.gz` |
| Linux x86_64 + C# | `codesearch-linux-x86_64-with-csharp.tar.gz` |
| macOS ARM64 | `codesearch-macos-arm64.tar.gz` |
| macOS ARM64 + C# | `codesearch-macos-arm64-with-csharp.tar.gz` |

Or build from source:

```bash
git clone https://github.com/flupkede/codesearch.git
cd codesearch
cargo build --release
```

### Index a repository

```bash
# Register and index the current repo (adds to ~/.codesearch/repos.json)
codesearch index add

# Register and index a repo from outside the repo folder
codesearch index add /path/to/my-project

# Incremental update (only changed files)
codesearch index /path/to/my-project

# Full rebuild
codesearch index /path/to/my-project --force
```

`codesearch index add` is intended to be run from inside the repo you want to register — pass the path explicitly if launched from elsewhere. First-time indexing takes 2–5 minutes; subsequent runs are incremental (10–30s) and branch switches re-index automatically. Use `codesearch index list/rm/prune` to manage registrations (see [Serve Mode](#serve-mode-multi-repo)).

## MCP Configuration

codesearch connects to AI agents via MCP. Two modes:

| Mode | How | Best for |
|------|-----|----------|
| **Local (stdio)** | `codesearch mcp` — single repo, auto-index + file watching | Working on one project |
| **Serve (HTTP)** | `codesearch serve` — multi-repo, TUI dashboard, lazy FSW | Multiple repos, cross-repo search |

### Local / Single Repo

The agent spawns `codesearch mcp` as a subprocess. It auto-detects the nearest index and starts a file watcher.

**OpenCode** — `~/.config/opencode/config.json`:

```json
{
  "mcp": {
    "codesearch": {
      "type": "local",
      "command": ["codesearch", "mcp"],
      "enabled": true
    }
  }
}
```

**Claude Code / Claude Desktop** — `~/.config/claude-code/config.json` or `claude_desktop_config.json` (identical schema):

```json
{
  "mcpServers": {
    "codesearch": {
      "command": "codesearch",
      "args": ["mcp"]
    }
  }
}
```

### Serve / Multi-Repo

Start the server first, then connect your agent. The server manages all registered repos with a TUI dashboard, lazy filesystem watchers, and idle eviction.

```bash
# Start the server (default port 39725)
codesearch serve
```

**OpenCode** — connect via HTTP:

```json
{
  "mcp": {
    "codesearch": {
      "type": "remote",
      "url": "http://127.0.0.1:39725/mcp",
      "enabled": true
    }
  }
}
```

**Claude Code / Claude Desktop** — force serve connection via `--mode client`:

```json
{
  "mcpServers": {
    "codesearch": {
      "command": "codesearch",
      "args": ["mcp", "--mode", "client"]
    }
  }
}
```

> **Note:** In multi-repo mode, agents must specify `project` or `group` in tool calls. `status` always works without scope. `get_chunk` auto-routes when the chunk_id is unique across repos; if ambiguous, it returns candidates and requires `project`.

### Agent Guidance (making agents use codesearch, not grep)

codesearch publishes **instructions** to every MCP client on connect (via the `initialize` handshake) — *when* to reach for codesearch vs grep/glob, *which* tool to pick, and the serve-mode caveats. Most clients (OpenCode, Cursor) surface these automatically.

If your agent skips codesearch and falls back to grep/glob too often, paste this quickstart into its rules (`AGENTS.md` for Claude Code/OpenCode, `.cursorrules` for Cursor):

> Prefer codesearch for semantic, cross-file, or symbol-oriented lookup ("where is X implemented", "find usages of Y", "how does Z flow"). Use plain grep/glob for a single known file, trivial one-line edits, or exact literal searches. In remote-serve mode, returned paths are from the **server's** filesystem — read content via `get_chunk` rather than opening paths locally, and unindexed dirs (`.venv`, `node_modules`, `build/`) simply return nothing.

OpenCode: put this in the user-level `~/.config/opencode/AGENTS.md` (applies across all projects). Claude Code reads a project-level `AGENTS.md`, so add it per-project (or symlink a shared one).

**Claude Code specifically** tends to ignore this advice more than other clients — its MCP tool schemas are deferred (an extra `ToolSearch` call is needed before codesearch tools are even callable), while Grep/Glob are always fully loaded and zero-friction, and spawned subagents don't inherit `AGENTS.md` or the MCP `initialize` instructions at all.

To make the preference **structural** instead of advisory, this repo ships three Claude Code `PreToolUse` hooks:

- **`grep-guard`** — on `Grep`. Blocks the first grep against an in-repo path when codesearch looks available (a local `.codesearch.db` at the git root, or a `CODESEARCH_SERVER` env var for remote-serve setups), with a message telling the model how to load and call codesearch instead. A retry of the same query within 5 minutes is let through unblocked — the legitimate "codesearch found nothing, falling back" path. Greps outside the current repo are never blocked, and the hook fails open (never traps the model).
- **`subagent-preamble`** — on `Agent` (the subagent-spawn tool). Prepends a short codesearch preamble to every subagent prompt, since subagents otherwise don't inherit `AGENTS.md` or MCP instructions at all.
- **`web-guard`** — on `WebSearch`/`WebFetch`. When you have remote documentation projects mounted (`codesearch remote mount`, e.g. `cloud/inriver`, `cloud/example-dam`), it blocks the first web call with guidance to search those indexed mounts first — often more precise and current than the open web. Same 5-minute retry-escape; when no mounts are configured it does nothing.

Install (idempotent — user scope applies to every project; `--project` is this repo only):

```bash
codesearch hooks claude install            # preferred — self-contained, all platforms
codesearch hooks claude install --project  # project scope (./.claude)
```

The native command embeds the hook scripts in the binary (no source tree needed) and merges the registrations into `settings.json`. The equivalent from-source installers still live in [`integrations/claude-code/`](integrations/claude-code/) (`install.ps1` / `install.sh`) if you'd rather run them directly.

Note: the grep-guard detects "codesearch is available **for this repo**" via a local `.codesearch.db` or `CODESEARCH_SERVER` — **not** by checking whether a `codesearch` process is running (that runs almost constantly as a multi-repo hub and would false-fire in every directory). For a remote-serve setup with no local index, set `CODESEARCH_SERVER` to opt back into enforcement.

## MCP Tools Reference

### `search` — Code Search

| Parameter | Type | Description |
|-----------|------|-------------|
| `query` | string | Natural language, code snippet, regex, or exact term |
| `mode` | `"semantic"` \| `"literal"` | Search backend (default: semantic) |
| `filter_path` | string | Path prefix filter (semantic mode) |
| `file_glob` | string | Glob filter (literal mode), e.g. `"src/**/*.rs"` |
| `language` | string | Language filter (literal mode) |
| `regex` | bool | Treat query as regex (literal mode) |
| `phrase` | bool | Exact phrase match (literal mode) |
| `compact` | bool | Metadata only, no code (default: true) |
| `limit` | int | Max results (default: 10 semantic, 20 literal) |
| `project` | string | Target specific repo (multi-repo) |
| `group` | string | Search across repo group (multi-repo) |

> **`filter_path` semantics by routing mode:**
> - **Local** project (stdio, or `project=<local-alias>`/local group on a `serve` hub): `filter_path`
>   is a **repo-relative** prefix (e.g. `src`, `docs/api`) — matched against the routed project's own
>   root, not the `<alias>/…` prefix shown in results.
> - **Federated / mounted** project (`project=<peer>/<alias>` or an `@peer` group fan-out): `filter_path`
>   is matched **client-side on the namespaced result path** (`<peer>/<alias>/…`) — i.e. exactly the
>   path you see in the results — because the peer only matches its own un-namespaced store paths. The
>   hub over-fetches from the peer and post-filters.
>
> Both modes now work without any client-side workaround; earlier releases dropped every hit when
> `filter_path` was combined with a serve-routed or federated project.

**Semantic mode** combines vector similarity (fastembed) + BM25 lexical scoring + exact identifier boosting, fused with RRF. Best for conceptual queries and mixed natural-language + symbol searches.

**Literal mode** uses Tantivy FTS. Use `regex=true` for patterns with punctuation (`foo::bar`, `Vec<T>`). Use `phrase=true` for multi-word exact matches.

### `find` — Symbol Navigation

| Parameter | Type | Description |
|-----------|------|-------------|
| `symbol` | string | Symbol name or file path (for imports) |
| `kind` | `"definition"` \| `"usages"` \| `"imports"` \| `"dependents"` | Navigation type |
| `definition_kind` | string | Filter: Function, Class, Method, Struct, Trait, Enum, Interface |
| `project` / `group` | string | Multi-repo routing |

### `explore` — File Exploration

| Parameter | Type | Description |
|-----------|------|-------------|
| `target` | string | File path (outline) or chunk_id (similar) |
| `kind` | `"outline"` \| `"similar"` | Exploration type |
| `limit` | int | Max results for similar mode |
| `project` / `group` | string | Multi-repo routing |

**Outline** returns all top-level symbols in a file (kind, signature, line range).
**Similar** finds semantically related chunks to a given chunk_id.

### `get_chunk` — Read Code

| Parameter | Type | Description |
|-----------|------|-------------|
| `chunk_id` | int | Chunk ID from search/explore results |
| `context_lines` | int | Extra lines before/after (0-20, default: 0) |
| `project` | string | Disambiguate if chunk_id exists in multiple repos |

In multi-repo mode: auto-routes when chunk_id is unique; returns candidates list when ambiguous.

### `find_impact` — Symbol Reference Impact

Find all call-sites and references to a symbol with file/line precision, powered by per-language semantic analysis. Currently supports **C#** (via the bundled `scip-csharp` helper).

| Parameter | Type | Description |
|-----------|------|-------------|
| `symbol_name` | string | Symbol name (e.g. `"FieldDefinition.Validate"`) |
| `file` | string | File path for position-based lookup |
| `line` | int | Line number for position-based lookup |
| `language` | string | Language hint (auto-detected from file extension) |
| `project` / `group` | string | Multi-repo routing |

Returns a list of references with `file`, `start_line`, `end_line`, and `kind` (e.g. `"call"`, `"definition"`). Exposes `index_age_seconds` so agents can reason about staleness.

> **Note:** Requires the `-with-csharp` release variant or a separately installed `scip-csharp` helper. See [C# Semantic Search](#c-semantic-search).

### `status` — Index Info

| Parameter | Type | Description |
|-----------|------|-------------|
| `kind` | `"index"` \| `"projects"` | What to query |
| `project` / `group` | string | Multi-repo routing |

## Serve Mode (Multi-Repo)

For working across multiple repositories simultaneously:

```bash
codesearch serve
```

This starts a background HTTP server with:
- **TUI dashboard** (ratatui) showing repo status, CPU usage, active sessions
- **Lazy filesystem watchers** — activated on first query per repo
- **Idle eviction** (30min) — unused repos are unloaded from memory
- **Session tracking** via MCP keep-alive

### TUI Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `↑` / `↓` | Navigate repo list |
| `i` | Show info overlay (chunks, files, model, DB size) |
| `d` | Run doctor diagnostics on selected repo |
| `f` | Force reindex selected repo |
| `r` | Remove selected repo (with confirmation dialog) |
| `s` | Reload repos config from disk |
| `q` | Quit serve |

### Repository Registration

Repos are registered via `codesearch index add`:

```bash
# Register a repo (creates index + adds to ~/.codesearch/repos.json)
codesearch index add /path/to/my-project

# Remove a repo
codesearch index rm /path/to/my-project

# List registered repos
codesearch index list

# Clean up stale entries (relocates moved repos, drops the rest)
codesearch index prune
```

The repository **alias** (the key in `repos.json`, used for groups and the MCP
`project` argument) is always derived automatically from the directory name —
there is no `--alias` flag.

Serve reads `~/.codesearch/repos.json` on startup and manages all registered repos.

#### Moved or renamed repositories

If you rename or move a registered folder, serve does **not** crash. On startup
it tries to **relocate** each missing repo automatically: it captures every
repo's git remote (`remote.origin.url`) at registration, and on a missing path
it scans nearby folders (bounded depth, override with
`CODESEARCH_RELOCATE_MAX_DEPTH`, default `3`) for a git checkout with the same
remote. A single unambiguous match is rewritten into `repos.json`; otherwise the
entry is logged and skipped (never indexed against a dead path). Run
`codesearch index prune` to relocate what can be relocated and drop the rest.

A hand-edited `repos.json` is also tolerated: empty entries, orphaned metadata,
and group references to unknown repos are cleaned up on load rather than
crashing.

### Groups

Groups let you search across related repositories:

```bash
codesearch groups add my-group --aliases repo1 repo2 repo3
codesearch groups list
```

Then in MCP tools: `group="my-group"` fans out the query to all repos in the group.

#### The `all` group

The name `all` is a **reserved virtual group** that always resolves to *every* registered repository — no setup required:

```
group="all"   # fans out to all repos, equivalent to listing every alias
```

- It is **not stored** in `repos.json` and always reflects the current set of registered repos (register or remove a repo and `all` updates automatically).
- It appears in `codesearch groups list` (marked `virtual`) and in the `scope_required` error's `available_groups`, so agents discover it without extra setup.
- It is **not the default** — when no `project`/`group` is specified in multi-repo mode, codesearch still returns `scope_required` (safe-by-default). Use `group="all"` explicitly when you want to search everywhere.
- `codesearch groups add all` and `codesearch groups remove all` are **rejected** — the name is reserved.

### Git Worktree Auto-Index

When using `git worktree add` to create parallel working directories, codesearch can auto-register new worktrees via a `post-checkout` git hook.

**Setup** (run inside any repo you want worktree auto-indexing for):

```bash
codesearch hooks git install
```

This writes a `post-checkout` hook to `.git/hooks/` that POSTs the worktree path to the running serve instance whenever a new worktree is checked out. The hook reads the serve URL from `~/.codesearch/serve_url` (automatically managed by `codesearch serve`).

**How it works:**
1. `codesearch serve` writes its URL to `~/.codesearch/serve_url` on startup (deletes on shutdown)
2. The `post-checkout` hook reads that file and POSTs the working directory to `POST /repos`
3. Serve registers the worktree path and begins indexing (deduped — won't re-register existing paths)

### Claude Code Guard Hooks

`codesearch hooks claude install` (`--project` for repo scope) installs the `PreToolUse` guard hooks that steer agents to codesearch before `Grep`/`WebSearch`/`WebFetch`. See [Agent Guidance](#agent-guidance-making-agents-use-codesearch-not-grep) above for what each guard does.

### MCP Connection Modes

The `codesearch mcp` command supports three modes:

| Mode | Behavior |
|------|----------|
| `auto` (default) | Connects to serve if running, otherwise local stdio |
| `client` | Always connects to serve, fails if not running |
| `local` | Always uses local DB (classic single-repo stdio) |

```bash
codesearch mcp --mode client  # force serve connection
```

The serve endpoint is available at `/mcp` (Streamable HTTP transport).

### Federation (remote peers)

codesearch can fan-out read queries (`search`, `get_chunk`) to **remote peers** — other `codesearch serve` instances (e.g. a cloud-hosted docs/KB peer) — and merge results with local indexes via RRF. This lets a team share one knowledge base while each dev keeps code search local.

**Manage peer entries** (pure local config — does not call the remote):

```bash
codesearch remote add cloud \
  --url https://codesearch-serve.<env>.<region>.azurecontainerapps.io \
  --api-key $API_KEY --timeout-secs 90
codesearch remote list          # show configured peers
codesearch remote rm cloud      # remove a peer entry
```

A group then references a peer via `@`-prefix (`"groups": { "docs": ["@cloud"] }`), and `group="docs"` fans the query out over TLS. Remote misses never hard-fail — they degrade to local-only results with a `warnings` field.

**Manage indexes ON a peer** — the same `index` verbs, scoped with `--remote <peer>`:

```bash
# list the repos living on the cloud peer
codesearch index list --remote cloud

# register a path on the peer's filesystem (NOT your local FS)
codesearch index add /data/docs/vendor-docs --remote cloud

# remove a repo by its alias on the peer (NOT a local path)
codesearch index rm inriver --remote cloud

# trigger a background reindex of one repo on the peer
codesearch index reindex inriver --remote cloud          # incremental
codesearch index reindex inriver --remote cloud --force  # force full
```

- `--remote` resolves `<peer>` against the peers you configured with `codesearch remote add`. An unknown peer produces a clear error listing the known ones.
- With `--remote`, `add` takes a **path on the peer's filesystem** and `rm`/`reindex` take a **remote alias** (never your local path).
- Without `--remote`, every `index` command behaves exactly as today (local).
- `index list` and `index reindex` accept `--json` for agent-friendly output (**requires `--remote`**).
- The write verbs (`add`, `reindex`, `--force`) require the peer to hold the repo **read-write**. A read-only / restore-only peer (e.g. a snapshot-restore cloud serve) rejects them — `--force` returns a clean HTTP 500 with a message, and `add` (a full embed) may OOM or time out a small replica. Use them against a writable peer; `list` is always safe.

**Per-vendor layout on a peer.** Instead of registering one mixed corpus, register each vendor's sub-folder as its own repo so the peer's layout mirrors your local one. (Requires a **writable** peer — see the note above; a read-only restore-only peer rejects `add`.)

```bash
for v in vendor-a-docs vendor-b-docs vendor-c-docs; do
  codesearch index add "/data/docs/$v" --remote cloud
done
codesearch index list --remote cloud   # one alias per vendor
```

**Mounting a peer's projects (opt-in, query one by name).** Adding a peer does **not** expose its projects automatically — you pick the individual indexes you want. Inspect what a peer offers, then mount the ones you care about:

```bash
codesearch remote available cloud          # list the peer's projects, ✓ marks mounted
codesearch remote mount cloud/akeneo       # opt in to a single index
codesearch remote mounts                   # show what you've mounted
codesearch remote unmount cloud/akeneo     # opt back out
```

A **mounted** project is addressable directly as `project=<peer>/<alias>` — a 1-to-1 passthrough to that peer's index:

```bash
# search only the peer's akeneo docs, by name
codesearch search "import products" --project cloud/akeneo
```

The `remote_mounts` allowlist in `~/.codesearch/repos.json` is the single source of truth: a **non-mounted** project is unroutable (even if the peer exposes it), and a whole-peer `@peer` group reference (e.g. `docs → [@cloud]`) federates **only your mounted indexes** for that peer, not its whole corpus. Mounted projects are surfaced by `list_projects` (a `remote_projects` array) and advertised in the `scope_required` error, so agents can discover them.

In the `codesearch serve` TUI, mounts appear in **italic/cyan**, distinguishing them from local indexes. Press `i` on a mount to see its **Remote Mount** info (peer URL + peer-reported status); the local-index actions (`doctor` / `reindex` / `remove`) are shown struck-through/disabled for a mount, since they act on a local index and a mount has none — manage a peer's indexes with `--remote` (above) instead.

## CLI Reference

| Command | Description |
|---------|-------------|
| `codesearch index [PATH]` | Index a repo (incremental; `--force` for full rebuild) |
| `codesearch search <QUERY>` | CLI search (for testing) |
| `codesearch mcp` | Start MCP stdio server |
| `codesearch serve` | Start multi-repo HTTP server with TUI |
| `codesearch stats` | Show database statistics |
| `codesearch clear` | Delete index |
| `codesearch doctor` | Health check (model, index, config) |
| `codesearch setup` | Download embedding models |
| `codesearch cache stats\|clear` | Manage embedding cache |
| `codesearch groups list\|add\|remove` | Manage repository groups |
| `codesearch remote add\|list\|rm` | Manage federation peers (`--url`, `--api-key`, `--group`, `--into-group`, `--timeout-secs`) |
| `codesearch remote available\|mount\|mounts\|unmount` | Inspect a peer's projects and opt-in mount them as `<peer>/<alias>` |
| `codesearch index ... --remote <peer>` | Run `index list\|add\|rm\|reindex` against a peer's filesystem instead of local |
| `codesearch hooks git install` | Install git post-checkout hook for worktree auto-indexing |
| `codesearch hooks claude install` | Install Claude Code codesearch-first guard hooks into settings.json |

## Configuration

### Environment Variables

| Variable | Description |
|----------|-------------|
| `CODESEARCH_SERVE_PORT` | Serve mode port (default: 39725) |
| `CODESEARCH_SERVE_API_KEY` | API key for management endpoints + all endpoints when serve binds to a non-localhost address (unset = no auth) |
| `CODESEARCH_ALLOWED_ROOTS` | Semicolon-separated allowed roots for repo registration (unset = all allowed) |
| `CODESEARCH_MCP_MODE` | MCP mode: auto, client, local |
| `CODESEARCH_REPOS_CONFIG` | Path to repos.json |
| `CODESEARCH_REPO_IDLE_TIMEOUT_SECS` | Idle eviction timeout (default: 1800) |
| `CODESEARCH_CACHE_MAX_MEMORY` | Embedding cache MB (default: 500) |
| `CODESEARCH_BATCH_SIZE` | Embedding batch size |
| `CODESEARCH_SCIP_CSHARP` | Override path to `scip-csharp` helper |
| `CODESEARCH_EXTENSION_MAP` | Path to the extension→language map (default: `~/.codesearch/extensions.json`) — see [Extension map](#extension-map) |
| `RUST_LOG` | Log level (e.g. `codesearch=debug`) |

### `.codesearchignore`

Place in repo root. Gitignore syntax. Excludes paths from indexing:

```gitignore
# Vendored code
vendor/
node_modules/
# Generated files
*.generated.cs
**/migrations/**
```

A **global** `.codesearchignore` can be placed at `~/.codesearch/.codesearchignore`. It applies to all repos with the lowest priority (repo-local `.codesearchignore`, `.gitignore`, and `.git/info/exclude` all override it). This is useful for patterns you want everywhere without modifying each repo.

### Extension map

By default codesearch recognises the fixed extension list in [Supported
Languages](#supported-languages); any other extension is `Unknown` and is
**skipped entirely** (never indexed). To teach codesearch about a non-standard
extension — or to deliberately remap a known one — drop a small JSON object at
`~/.codesearch/extensions.json` mapping extension → language name:

```json
{
  "inc": "php",
  "phtml": "php",
  "h": "cpp"
}
```

- Keys are file extensions, with or without a leading dot, case-insensitive
  (`"inc"`, `".inc"`, `".INC"` are equivalent). Only the **last** dot-suffix is
  matched, so `Foo.class.inc` maps via `"inc"`.
- Values are language names — the names from the table above plus common aliases
  (`php`, `cpp`/`c++`, `csharp`/`c#`, `golang`, `js`, `ts`, …), case-insensitive.
- The map applies to **all** indexed repos, and user entries **take precedence**
  over the built-ins (so `"h": "cpp"` overrides the default C mapping).
- It's fully optional and fail-safe: a missing, empty, or malformed file simply
  means "no overrides" and is logged, never fatal. Unknown language names are
  skipped with a warning.
- Set `CODESEARCH_EXTENSION_MAP` to load the file from a different path.

> This is the supported answer to "my PHP is in `*.inc` files" (issue #138):
> `.inc` is intentionally not a built-in because it's language-agnostic
> (assembly, SQL, C/PHP includes all use it), so you opt in per your codebase.

### `repos.json`

Located at `~/.codesearch/repos.json`. Managed by `codesearch index add/rm`. Contains repo aliases → paths and group definitions. See [Serve Mode](#serve-mode-multi-repo).

## Security

This section documents the security model of **federation / remote peers** — the largest network surface in codesearch — followed by serve access control.

### Serve access control

When `codesearch serve` is exposed beyond a single trusted user (shared dev machines, a network bind), two environment variables harden access:

- **`CODESEARCH_SERVE_API_KEY`** — gates access depending on how serve is bound:
  - **Non-localhost bind** (`--host 0.0.0.0`, a LAN IP, etc.) — **ALL endpoints** require the key (health, status, MCP search, and management). Setting this variable is *required* when binding to a non-localhost address; serve refuses to start without it.
  - **Localhost bind** (default) — only **management endpoints** (`POST /repos`, `DELETE /repos/:alias`, `POST /repos/:alias/reindex`, `POST /reload`) require the key. Health, status, and MCP search remain open.
  - Send the key on every request via `Authorization: Bearer <key>` or `X-API-Key: <key>`.
  - **Client side:** `codesearch index add/rm/reindex` delegate to a running serve, so the CLI must send the same key. Set `CODESEARCH_SERVE_API_KEY` in the client's environment (same value as the server) and the CLI attaches it automatically. Without it, delegation returns `401` and falls back to local indexing — which risks LMDB file-lock conflicts if serve is still running.
- **`CODESEARCH_ALLOWED_ROOTS`** — semicolon-separated list of filesystem roots. Repo registration is rejected for paths outside these roots. Prevents indexing arbitrary directories.

Both are backward compatible: unset means no restriction (on a localhost bind).

### Federation security model

Federation is **operator-to-operator**, not end-user-facing. The only inputs that decide *where* requests go and *which key* they carry are the peer entries you register locally with `codesearch remote add` (stored in `~/.codesearch/repos.json`). No search query, MCP argument, or remote response ever becomes a request target or selects a key.

- **Trust model.** Peers see your search queries (they must, to answer them) and return chunks. Register only peers whose operator you trust with that query visibility.
- **No SSRF from queries.** Outbound calls go solely to URLs you configured; a search term cannot redirect the client to an attacker host.

#### Secrets

- **Storage.** Each peer's `api_key` is stored in **plaintext** in `~/.codesearch/repos.json` (your home directory, outside any git repo — it is never committed). Protect it with filesystem permissions, the same way you would an SSH key or a `.env` file. On the server side, inject the key from a secret store — the reference cloud deployment sets it as an Azure Container Apps secret (`secretref:api-key`), so it never sits in the image or in source.
- **Transport.** The key is sent only as an `Authorization: Bearer <key>` header, over HTTPS. Use `https://` peer URLs; the federation client uses **rustls with certificate verification enabled** — there is no certificate-bypass (`danger_accept_invalid_certs`) anywhere in the codebase.
- **Never in URLs or logs.** The key is not appended to query strings and is not written to logs; it lives only in the `Authorization` header of the in-flight request.

#### Redirects

The HTTP client follows up to 10 redirects by default (reqwest's standard policy) but **strips the `Authorization` header on cross-host redirects**. A peer that answers with a `3xx` to a different host therefore cannot exfiltrate the bearer token there; same-host redirects (e.g. path canonicalisation) preserve it. *Defense-in-depth:* if you suspect a peer's host or DNS has been compromised, rotate that peer's key.

#### Serve-side enforcement

The remote serve instance enforces its own auth on every inbound request from federation:

- **Non-localhost bind** (any cloud/LAN deployment): **all** endpoints — including `/search` and `/chunk/:id` — require the key. The reference cloud peer binds `0.0.0.0`, so every federation fan-out carries the key and is rejected without it.
- **Management endpoints** (`POST /repos`, `DELETE /repos/:alias`, `POST .../reindex`, `POST /reload`) require the key on every bind, localhost included.

The `@peer` fan-out attaches the configured key via the same `bearer_auth` path the local CLI uses, so it satisfies both layers automatically.

#### Write verbs (`index … --remote`)

`add`, `rm`, and `reindex` against a peer are **authenticated management calls**. They carry the key like any other request and the *peer* decides whether to act (a read-only / restore-only peer rejects writes — see [Federation](#federation-remote-peers)). The local CLI only sends a path or alias for the peer to act on; it executes nothing on your machine and writes nothing to your filesystem.

#### Cross-instance isolation

On fan-out, the client strips the local `project` and forces `group` to the value configured for that peer (or `all`). Projects are local to each instance, so this prevents one instance's project names from leaking into another's query namespace.

## C# Semantic Search

All C#-specific setup, operation, installation, and testing lives in [README_CSharp.md](README_CSharp.md).

If you do not work with C# repos, you can skip it entirely.

## Supported Languages

Tree-sitter AST-aware chunking:

| Language | Extensions |
|----------|-----------|
| Rust | `.rs` |
| Python | `.py`, `.pyw`, `.pyi` |
| JavaScript | `.js`, `.mjs`, `.cjs` |
| TypeScript | `.ts`, `.tsx`, `.jsx`, `.mts`, `.cts` |
| C | `.c`, `.h` |
| C++ | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hxx` |
| C# | `.cs` |
| Go | `.go` |
| Java | `.java` |
| Dart | `.dart` |
| Shell | `.sh`, `.bash`, `.zsh` |
| Ruby | `.rb`, `.rake` |
| PHP | `.php` |
| YAML | `.yaml`, `.yml` |
| JSON | `.json` |
| Markdown | `.md`, `.markdown`, `.txt` |
| Jupyter | `.ipynb` |

Markdown uses the tree-sitter-md **block** grammar — chunks align to sections,
headings, and code fences. Jupyter notebooks are parsed as JSON; code and
markdown cells are extracted, tagged with `[code]` or `[markdown]`, and
adjacent same-type cells under 50 lines are merged into single chunks.
All other text files use line-based chunking as fallback.

Files whose extension isn't recognised are treated as `Unknown` and **skipped
entirely** (not indexed). If your codebase uses a non-standard extension for a
supported language — e.g. legacy PHP in `*.inc` files, or `.h` you want parsed
as C++ — map it explicitly with the [extension map](#extension-map).

## Core Technology

| Component | Technology |
|-----------|-----------|
| Embedding | fastembed + ONNX Runtime (CPU) |
| Vector store | arroy (Approximate Nearest Neighbors) + LMDB |
| Full-text search | Tantivy (BM25, AND mode) |
| Chunking | Tree-sitter AST parsing |
| Incremental sync | SHA-256 content hashing |
| Caching | 3-layer: in-memory (Moka) → persistent disk → query cache |
| Schema | Versioned via `metadata.json` |

## Development

```bash
# Build
cargo build

# Run tests
cargo test

# Check + lint
cargo clippy --all-targets -- -D warnings

# Format
cargo fmt --all
```

## License

Apache-2.0

## Acknowledgements

This project is a fork of [demongrep](https://github.com/yxanul/demongrep) by [yxanul](https://github.com/yxanul). Huge thanks for building such a solid foundation.

Built with: [fastembed-rs](https://github.com/Anush008/fastembed-rs), [arroy](https://github.com/meilisearch/arroy), [tantivy](https://github.com/quickwit-oss/tantivy), [tree-sitter](https://tree-sitter.github.io/), [ratatui](https://github.com/ratatui/ratatui), [LMDB](http://www.lmdb.tech/).
