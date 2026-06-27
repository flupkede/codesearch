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

Other projects in the same niche may go deeper on call-graph traversal, polished standalone CLIs, or memory/knowledge-graph features. codesearch is intentionally narrower — it picks "lightweight, multi-repo, MCP-native, fully offline" and stays on that lane.

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

    subgraph "Serve Mode (multi-repo)"
        ServeRouter[HTTP Router] -->|project/group routing| Repo1[Repo A]
        ServeRouter --> Repo2[Repo B]
        ServeRouter --> RepoN[Repo N]
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

# Remove a repo
codesearch index rm /path/to/my-project

# List registered repos
codesearch index list

# Remove stale entries (relocates moved repos first, then drops the rest)
codesearch index prune
```

`codesearch index add` is intended to be run from inside the repo you want to register.
If you're launching it from somewhere else, pass the repo path explicitly.

First-time indexing takes 2–5 minutes. Subsequent runs are incremental (10–30s). Branch switches trigger automatic re-indexing.

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

**Claude Code** — `~/.config/claude-code/config.json`:

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

**Claude Desktop** — `claude_desktop_config.json`:

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

codesearch publishes **instructions** to every MCP client on connect (via the `initialize` handshake). These tell the agent *when* to reach for codesearch vs grep/glob, *which* tool to pick, and the service-mode caveats (paths are from the server's filesystem; not every directory is indexed). Most clients (OpenCode, Cursor) surface these automatically.

If your agent **skips codesearch** and falls back to grep/glob too often, add this quickstart to its project rules (`AGENTS.md` for Claude Code / OpenCode, `.cursorrules` for Cursor, custom instructions for others):

```markdown
## Codesearch quickstart

Prefer codesearch over manual grep/glob for:
- semantic, cross-file, or symbol-oriented lookup
- cases where you don't know the exact file path
- "where is X implemented", "find usages of Y", "how does Z flow through the code"

Use plain grep/glob instead for:
- a single known file
- trivial one-line edits
- exact literal searches

When codesearch runs as a remote service (`codesearch serve` on another host),
the paths it returns are from the SERVER's filesystem. Use the `get_chunk` tool
to read content — don't try to open returned paths locally. Not every directory
is indexed (e.g. `.venv`, `node_modules`, `build/`); if a search returns nothing,
the dir may simply be unindexed.
```

> **OpenCode users:** the user-level `~/.config/opencode/AGENTS.md` is the right place for this — it applies across all projects. Claude Code reads a project-level `AGENTS.md` instead, so add the quickstart per-project (or symlink a shared one).

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
codesearch hook install
```

This writes a `post-checkout` hook to `.git/hooks/` that POSTs the worktree path to the running serve instance whenever a new worktree is checked out. The hook reads the serve URL from `~/.codesearch/serve_url` (automatically managed by `codesearch serve`).

**How it works:**
1. `codesearch serve` writes its URL to `~/.codesearch/serve_url` on startup (deletes on shutdown)
2. The `post-checkout` hook reads that file and POSTs the working directory to `POST /repos`
3. Serve registers the worktree path and begins indexing (deduped — won't re-register existing paths)

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
codesearch index add /data/docs/aprimo-docs --remote cloud

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
for v in aprimo-docs inriver-docs akeneo-docs; do
  codesearch index add "/data/docs/$v" --remote cloud
done
codesearch index list --remote cloud   # one alias per vendor
```

See `docs/federation-feature.md` (Rust feature: REST endpoints, RRF merge, config) and `docs/federation-cloud-deployment.md` (Azure deployment + the `--remote` management recipe).

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
| `codesearch hook install` | Install git post-checkout hook for worktree auto-indexing |

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
| `RUST_LOG` | Log level (e.g. `codesearch=debug`) |

### Security

When `codesearch serve` is exposed beyond a single trusted user (e.g. shared dev machines, a network bind), two environment variables harden access:

- **`CODESEARCH_SERVE_API_KEY`** — gates access depending on how serve is bound:
  - **Non-localhost bind** (`--host 0.0.0.0`, a LAN IP, etc.) — **ALL endpoints** require the key (health, status, MCP search, and management). Setting this variable is *required* when binding to a non-localhost address; serve refuses to start without it.
  - **Localhost bind** (default) — only **management endpoints** (`POST /repos`, `DELETE /repos/:alias`, `POST /repos/:alias/reindex`, `POST /reload`) require the key. Health, status, and MCP search remain open.
  - Send the key on every request via `Authorization: Bearer <key>` or `X-API-Key: <key>`.
  - **Client side:** `codesearch index add/rm/reindex` delegate to a running serve, so the CLI must send the same key. Set `CODESEARCH_SERVE_API_KEY` in the client's environment (same value as the server) and the CLI attaches it automatically. Without it, delegation returns `401` and falls back to local indexing — which risks LMDB file-lock conflicts if serve is still running.
- **`CODESEARCH_ALLOWED_ROOTS`** — semicolon-separated list of filesystem roots. Repo registration is rejected for paths outside these roots. Prevents indexing arbitrary directories.

Both are backward compatible: unset means no restriction (on a localhost bind).

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

### `repos.json`

Located at `~/.codesearch/repos.json`. Managed by `codesearch index add/rm`. Contains repo aliases → paths and group definitions. See [Serve Mode](#serve-mode-multi-repo).

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
