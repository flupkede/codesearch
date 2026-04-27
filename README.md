# codesearch

**Local semantic code search — MCP server for AI agents.**

codesearch indexes your codebase with vector embeddings and exposes it via the Model Context Protocol (MCP), so AI agents like OpenCode, Claude Code, and Claude Desktop can search your code semantically without sending anything to external APIs.

> Fork of [demongrep](https://github.com/yxanul/demongrep) by yxanul — extended with incremental indexing, multi-repo support, MCP integration, and more.

---

## How It Works — Three Modes

```
+-------------------------------------------------------------+
|  LOCAL / AUTO  (single repo, agent runs inside the project) |
|                                                             |
|  OpenCode / Claude Code                                     |
|       |  stdio                                              |
|       +---> codesearch mcp ---> .codesearch.db (local)     |
+-------------------------------------------------------------+

+-------------------------------------------------------------+
|  CLIENT + SERVE  (multi-repo, Claude Desktop)               |
|                                                             |
|  Claude Desktop                                             |
|       |  stdio                                              |
|       +---> codesearch mcp --mode client                   |
|                 |  HTTP (Streamable MCP)                    |
|                 +---> codesearch serve ---> repos.json      |
|                           +---> /project-a/.codesearch.db  |
|                           +---> /project-b/.codesearch.db  |
+-------------------------------------------------------------+
```

### Why `codesearch serve`?

Claude Desktop opens without a project context — it is not tied to any specific folder. This means `codesearch mcp` in local mode cannot discover a database because there is no current working directory to search from.

The solution: run `codesearch serve` as a persistent background process that holds connections to one or more indexed repositories. Claude Desktop then connects through `codesearch mcp --mode client`, which acts as a transparent stdio-to-HTTP bridge between Claude Desktop and the serve instance.

This also unlocks **multi-repo search**: a single serve instance manages multiple projects, and agents can search across all of them in one call using groups.

### MCP Mode Selection

`codesearch mcp` accepts a `--mode` flag:

| Mode | Behavior |
|---|---|
| `auto` (default) | Probes for a running serve instance. If found, proxies to it. If not, falls back to local mode. |
| `local` | Always uses a local database. Ignores any running serve instance. Use this for OpenCode and Claude Code running inside a project. |
| `client` | Always connects to serve. Fails with a clear error if serve is not running. Use this for Claude Desktop. |

---

## Installation

### Pre-built Binary (Recommended)

Download from [Releases](https://github.com/flupkede/codesearch/releases):

| Platform | File |
|---|---|
| Windows x86_64 | `codesearch-windows-x86_64.zip` |
| Linux x86_64 | `codesearch-linux-x86_64.tar.gz` |
| macOS Apple Silicon | `codesearch-macos-arm64.tar.gz` |

Place the binary on your `PATH` and verify:

```bash
codesearch --version
codesearch doctor
```

### Build from Source

```bash
git clone https://github.com/flupkede/codesearch.git
cd codesearch
cargo build --release
# binary: target/release/codesearch (or .exe on Windows)
```

---

## Indexing

Before searching, index your codebase. The index is placed automatically at the git repository root.

```bash
cd /path/to/your/project
codesearch index
```

For multi-repo setups, index each repo and register it:

```bash
# Index and register with an alias
codesearch index add /path/to/project-a --alias project-a
codesearch index add /path/to/project-b --alias project-b

# List registered repos
codesearch index list
```

| Command | Description |
|---|---|
| `codesearch index` | Incremental re-index (only changed files) |
| `codesearch index --force` | Full rebuild from scratch |
| `codesearch index add [PATH] [--alias NAME]` | Create index and register in `repos.json` |
| `codesearch index rm [PATH]` | Remove index and unregister |
| `codesearch index list` | Show all registered repos and groups |

**Incremental updates:** After the first index, only changed files are re-processed. A file watcher keeps the index current during active agent sessions.

---

## `codesearch serve`

Start a persistent MCP HTTP server that manages multiple repositories:

```bash
codesearch serve
# Listens on http://127.0.0.1:39725 by default

codesearch serve --port 8080

# Register repos that aren't in repos.json yet
codesearch serve --register /path/to/project-a --register /path/to/project-b
```

| Option | Default | Description |
|---|---|---|
| `--port` / `-p` | 39725 | Port (or `CODESEARCH_SERVE_PORT` env var) |
| `--register` / `-r` | — | Register repo paths at startup (repeatable) |

**Endpoints:**

| Method | Path | Description |
|---|---|---|
| GET | `/health` | Returns `{"codesearch_server": true, "version": "..."}` |
| POST | `/mcp` | MCP Streamable HTTP endpoint |

**Live behavior:**
- Each registered repo gets its own file watcher. File edits and git branch switches are picked up automatically.
- Changes to `repos.json` (adding/removing repos) are detected on the next tool call without restarting serve.
- Strict single-writer lock: at most one process writes to a database at a time.

**Running as a background service:** On Windows, use Task Scheduler or NSSM. On Linux/macOS, use a systemd unit or launchd plist. Keep it running as long as you use Claude Desktop.

---

## MCP Configuration

### OpenCode — single repo

OpenCode runs inside your project directory, so local mode works perfectly.

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

No project path required — codesearch auto-detects the database from the current working directory.

### Claude Code — single repo

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

### Claude Desktop — via serve (recommended)

Claude Desktop has no project context, so it must connect through a running serve instance.

**Step 1:** Start serve (once, keep it running):

```bash
codesearch serve
```

**Step 2:** Configure Claude Desktop (`claude_desktop_config.json`):

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

`--mode client` tells codesearch to always connect to serve and fail with a clear error if serve is not running, rather than silently falling back to a useless local mode with no database.

### Direct HTTP (OpenCode remote, other MCP clients)

Any MCP client that supports Streamable HTTP can connect directly to serve without the stdio proxy:

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

---

## Groups — Cross-Repo Search

Groups let you search across multiple related repositories in a single MCP tool call. Results are returned with alias-prefixed paths so the agent knows which repo each result comes from.

**Create and manage groups:**

```bash
codesearch groups add platform --aliases shared-lib service-a service-b
codesearch groups list
codesearch groups rm platform
```

**Config format** (`~/.codesearch/repos.json`):

```json
{
  "repos": {
    "shared-lib": "/projects/shared-lib",
    "service-a":  "/projects/service-a",
    "service-b":  "/projects/service-b"
  },
  "groups": {
    "platform": ["shared-lib", "service-a", "service-b"]
  }
}
```

**Using groups from an agent:**

```
search(mode="semantic", group="platform", query="where is the auth token validated?")
```

Results look like:

```json
[
  { "path": "shared-lib/src/auth.rs", "line": 42 },
  { "path": "service-a/src/middleware.rs", "line": 17 }
]
```

The serve instance fans out the query to all repos in the group, fuses the ranked results with RRF, and prefixes each path with the repo alias. No config changes are needed on the agent side — just pass `group="platform"` in any tool call.

Groups are created and managed by you. AI agents use them but do not create them.

---

## MCP Tools

The tool surface is five consolidated tools:

| Tool | Key Parameters | Description |
|---|---|---|
| `search` | `query`, `mode` (`"semantic"` / `"literal"`), `limit`, `project`, `group` | Unified code search. Semantic uses vector + BM25 + RRF. Literal is pure FTS with regex/phrase support. |
| `find` | `kind` (`"definition"` / `"usages"` / `"imports"` / `"dependents"`), `symbol`, `project`, `group` | Symbol navigation: find where something is defined, where it is used, what it imports, what depends on it. |
| `explore` | `kind` (`"outline"` / `"similar"`), `target`, `project` | File outline (all symbols in a file) or similar chunk search by chunk ID. |
| `get_chunk` | `chunk_id`, `context_lines` (0–20) | Full source of a specific chunk, with optional surrounding lines. |
| `status` | `kind` (`"index"` / `"projects"`) | Index health and stats, or list all registered repos and groups. |

All tools accept `project` (route to a specific repo by alias) and `group` (fan-out across a group) when connected to serve.

### Typical Agent Workflow

```
search("auth handler", compact=true)       --> 20 results, ~800 tokens
find(kind="usages", symbol="authenticate") --> 8 call sites, ~100 tokens
get_chunk(chunk_id=1234, context_lines=5)  --> exact function body
```

Compact mode (default) returns only metadata: path, line range, kind, signature, score. Use `get_chunk` to read full code. This keeps token usage 90%+ lower than returning full file contents.

---

## Searching (CLI)

```bash
codesearch search <QUERY> [OPTIONS]
```

| Option | Short | Default | Description |
|---|---|---|---|
| `--max-results` | `-m` | 25 | Maximum results |
| `--per-file` | | 1 | Max matches per file |
| `--content` | `-c` | | Show full chunk content |
| `--scores` | | | Show relevance scores and timing |
| `--compact` | | | File paths only (like `grep -l`) |
| `--sync` | `-s` | | Re-index changed files before searching |
| `--json` | | | JSON output for scripting |
| `--filter-path` | | | Restrict to path (e.g. `src/api/`) |
| `--vector-only` | | | Vector similarity only, no BM25 |
| `--rerank` | | | Neural reranking (~1.7s extra, higher accuracy) |
| `--rrf-k` | | 20 | RRF fusion parameter |
| `--create-index` | | `true` | Auto-create index if none exists |

```bash
codesearch search "database connection pooling"
codesearch search "error handling" --content --rerank
codesearch search "validation" --filter-path src/api --json -m 10
codesearch search "auth logic" --sync   # re-index changed files first
```

---

## Supported Languages

**Full AST chunking** (tree-sitter): Rust, Python, JavaScript, TypeScript, C, C++, C#, Go, Java

**Line-based chunking**: Ruby, PHP, Swift, Kotlin, Shell, Markdown, JSON, YAML, TOML, SQL, HTML, CSS/SCSS

---

## Embedding Models

| Model | ID | Dims | Notes |
|---|---|---|---|
| MiniLM-L6 (quantized) | `minilm-l6-q` | 384 | **Default** — fastest |
| BGE Small (quantized) | `bge-small-q` | 384 | Good general quality |
| BGE Base | `bge-base` | 768 | Higher quality |
| Jina Code | `jina-code` | 768 | Code-optimized |
| Nomic v1.5 | `nomic-v1.5` | 768 | Long context |

The model used for indexing is stored in the database metadata. Always search with the same model you indexed with, or rebuild with `--force` when switching models.

Pre-download models before first use:

```bash
codesearch setup
codesearch setup --model jina-code
```

---

## Troubleshooting

| Problem | Solution |
|---|---|
| "No database found" | Run `codesearch index` in your project directory |
| Claude Desktop times out on connect | Make sure `codesearch serve` is running before starting Claude Desktop |
| serve not finding a repo | Check `repos.json` with `codesearch index list`; re-register with `codesearch index add` |
| Search results stale after branch switch | File watcher handles this automatically; check serve logs if it doesn't |
| Port conflict | `codesearch serve --port 8080` or set `CODESEARCH_SERVE_PORT=8080` |
| Multiple `.git` detected error | Index from the repository root, not a parent containing multiple repos |
| Model mismatch warning | Re-index: `codesearch index --force` |

### Logging

```bash
# In MCP config (Claude Desktop):
"args": ["mcp", "--mode", "client", "--loglevel", "debug"]

# Serve logs:
codesearch serve --loglevel debug

# Log files:
# <project>/.codesearch.db/logs/codesearch.log
```

---

## License

Apache-2.0

## Acknowledgements

Fork of [demongrep](https://github.com/yxanul/demongrep) by [yxanul](https://github.com/yxanul). Thanks for building such a solid foundation.
