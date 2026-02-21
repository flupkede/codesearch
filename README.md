# codesearch

**Token-efficient MCP server for AI agents — local semantic code search powered by Rust.**

codesearch is designed as the primary bridge between AI agents and your codebase. It provides a Model Context Protocol (MCP) server that enables OpenCode, Claude Code, and other AI assistants to perform intelligent, semantic code searches with minimal token usage — all running locally with no API calls.

**Use AI to understand your code:** Query your codebase with natural language like *"where do we handle authentication?"* or *"show me all API endpoints"* and get instant, accurate results.

> **Fork notice:** This project is a fork of [demongrep](https://github.com/yxanul/demongrep) by [yxanul](https://github.com/yxanul). Huge thanks to yxanul for creating the original project — it's an excellent piece of work and the foundation everything here builds on. Some features (like global database support) were contributed back to demongrep via PR. codesearch extends it further with incremental indexing, MCP token optimizations, AI agent integration, and more.

---

## Features

### 🤖 MCP Server (Primary Use Case)

- **Token-Efficient AI Integration** — Compact responses minimize token usage in AI conversations
- **OpenCode Compatible** — Seamless integration with OpenCode and other MCP-compatible agents
- **Automatic Index Discovery** — Finds your codebase index automatically from any directory
- **Real-Time Updates** — File watcher and git branch detection keep index current during AI sessions
- **Privacy-First** — All processing local, no code leaves your machine, no external API calls

### 🔍 Core Search Capabilities

- **Semantic Search** — Natural language queries that understand code meaning
- **Hybrid Search** — Vector similarity + BM25 full-text search with RRF fusion
- **Neural Reranking** — Optional cross-encoder reranking for higher accuracy
- **Smart Chunking** — Tree-sitter AST-aware chunking that preserves functions, classes, methods
- **Incremental Indexing** — Only re-indexes changed files (10–100× faster updates)
- **Embedding Cache** — Three-layer caching system for dramatically faster subsequent indexes
- **Git-Aware Index Placement** — Automatically places indexes at git repository roots
- **Automatic Branch Detection** — Detects git branch changes and refreshes the index
- **Global & Local Indexes** — Per-project local indexes or a shared global index
- **Fast** — Sub-second search after initial model load

---

## Table of Contents

- [Installation](#installation)
- [Quick Start for MCP](#quick-start-for-mcp)
- [Indexing](#indexing)
- [Git Integration](#git-integration)
- [Embedding Cache](#embedding-cache)
- [Searching](#searching)
- [MCP Server Configuration](#mcp-server-configuration)
- [Other Commands](#other-commands)
- [Search Modes](#search-modes)
- [Global vs Local Indexes](#global-vs-local-indexes)
- [Supported Languages](#supported-languages)
- [Embedding Models](#embedding-models)
- [Configuration](#configuration)
- [How It Works](#how-it-works)
- [Troubleshooting](#troubleshooting)

---

## Installation

### 📥 Download Pre-built Binary (Recommended)

The fastest way to get started - download a single executable ready to use. No dependencies, no build process, just extract and run.

Download the latest release for your platform from [Releases](https://github.com/flupkede/codesearch/releases):

| Platform | Download |
|---|---|
| **Windows x86_64** | `codesearch-windows-x86_64.zip` |
| **Linux x86_64** | `codesearch-linux-x86_64.tar.gz` |
| **macOS (Apple Silicon)** | `codesearch-macos-arm64.tar.gz` |

Extract and place the binary somewhere on your `PATH`:

**Windows (PowerShell):**
```powershell
# Extract zip
Expand-Archive codesearch-windows-x86_64.zip
# Add to PATH or move to directory on PATH
$env:Path += ";$PWD"
```

**Linux/macOS:**
```bash
# Extract tar.gz
tar -xzf codesearch-linux-x86_64.tar.gz  # or codesearch-macos-arm64.tar.gz
# Move to PATH
sudo mv codesearch /usr/local/bin/
# Verify installation
codesearch --version
```

### 🔨 Building from Source

If you prefer to build from source or need a custom build, you'll need Rust and a few dependencies.

#### Prerequisites

| Platform | Command |
|---|---|
| **Rust** | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` |
| **Ubuntu/Debian** | `sudo apt-get install -y build-essential protobuf-compiler libssl-dev pkg-config` |
| **Fedora/RHEL** | `sudo dnf install -y gcc protobuf-compiler openssl-devel pkg-config` |
| **macOS** | `brew install protobuf openssl pkg-config` |
| **Windows** | `winget install -e --id Google.Protobuf` or `choco install protoc` |

#### Build Steps

```bash
git clone https://github.com/flupkede/codesearch.git
cd codesearch

# Build release binary
cargo build --release

# Binary location:
#   Linux/macOS: target/release/codesearch
#   Windows:     target\release\codesearch.exe

# Optionally add to PATH:
# Linux/macOS:
sudo cp target/release/codesearch /usr/local/bin/
# Windows (PowerShell, as admin):
Copy-Item target\release\codesearch.exe "$env:LOCALAPPDATA\Microsoft\WindowsApps\"
```


### Verify Installation

```bash
codesearch --version
codesearch doctor
```

---

## Quick Start for MCP

Get up and running with AI agents in under 2 minutes.

### 1️⃣ Install codesearch

Download the pre-built binary for your platform from [Releases](https://github.com/flupkede/codesearch/releases) and extract it to your PATH, or build from source (see [Installation](#installation)).

### 2️⃣ Index your codebase

```bash
cd /path/to/your/project

# First time: creates index at git root (~2-5 min, depends on codebase size)
codesearch index
```

The index is automatically placed at the git repository root, so it works from any subdirectory.

### 3️⃣ Configure your AI agent

**For OpenCode:**
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

**For Claude Code Desktop:**
Add to `claude_desktop_config.json` (Windows) or `claude_desktop_config.json` (macOS/Linux):
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

### 4️⃣ Start using AI to understand your code

Restart your AI agent and start asking questions:
- *"Where is the authentication logic?"*
- *"Show me all API endpoints"*
- *"How do we handle errors in this project?"*

The AI agent will use codesearch to find relevant code and provide accurate answers with minimal token usage.

---

## Quick Start for CLI

```bash
# 1. Navigate to your project
cd /path/to/your/project

# 2. Index the codebase (first time ~30–60s, incremental afterwards)
codesearch index

# 3. Search with natural language
codesearch search "where do we handle authentication?"
```

---

## Indexing

Indexing is the core operation — it parses your code into semantic chunks, generates embeddings, and stores them for fast retrieval.

```bash
codesearch index [PATH] [OPTIONS]
```

| Option | Short | Description |
|---|---|---|
| `--force` | `-f` | Delete existing index and rebuild from scratch (alias: `--full`) |
| `--dry-run` | | Preview what would be indexed |
| `--add` | | Create a new index (combine with `-g` for global) |
| `--global` | `-g` | Target the global index (with `--add`) |
| `--rm` | | Remove the index (alias: `--remove`) |
| `--list` | | Show index status |
| `--model` | | Override embedding model |

### Incremental Indexing

When an index already exists, `codesearch index` only processes changed, added, and deleted files — typically 10–100× faster than a full rebuild.

```bash
codesearch index           # Incremental (default)
codesearch index --force   # Full rebuild
codesearch index list      # Show index status
```

### What Gets Indexed

All text files are included, respecting `.gitignore` and `.codesearchignore`. Binary files, `node_modules/`, `.git/`, etc. are skipped automatically.

See [Global vs Local Indexes](#global-vs-local-indexes) for where the index is stored.

---

## Git Integration

codesearch is deeply integrated with git for intelligent index management and automatic updates.

### Automatic Git Root Detection

When you run `codesearch index`, the index is automatically placed at the **git repository root** (where `.git/` is located), regardless of your current working directory within the project.

```bash
cd /projects/myapp/src/api/
codesearch index  # Creates .codesearch.db/ at /projects/myapp/
```

**How it works:**
- Searches upward from the current directory to find `.git/` or `.git` (worktree) file
- Places `.codesearch.db/` at the same level as the git repository
- Detects nested git worktrees and errors on multiple child `.git` directories
- Falls back to current directory if no git repository is found

This ensures a **single, authoritative index per git repository**, avoiding confusion from multiple indexes in subdirectories.

---

### Automatic Branch Change Detection

codesearch monitors `.git/HEAD` in real-time and automatically refreshes the index when you switch branches.

```bash
# Currently on main branch
codesearch index

# Switch branches
git checkout feature/new-auth

# Index is automatically refreshed to reflect the new branch files
```

**Behavior:**
- The MCP server (and `codesearch serve`) polls `.git/HEAD` every 100ms
- Detects HEAD changes (branch switches) and triggers an incremental re-index
- Updates happen automatically in the background — no manual intervention needed

This is especially useful when working with different branches in AI coding sessions — the search results always reflect your current branch state.

### Database Bloat Monitoring

`codesearch stats` now shows a **bloat ratio** that indicates how much free space exists in the LMDB database:

```bash
$ codesearch stats
Database: .codesearch.db/
Files: 1,234
Chunks: 45,678
Bloat ratio: 1.2  # 1.2x size indicates 20% free space available
```

- **Bloat ratio < 1.5**: Healthy, no action needed
- **Bloat ratio > 2.0**: Consider compacting (future feature)

The bloat ratio is calculated from LMDB's internal statistics and helps monitor database health over time.

---

## Embedding Cache

codesearch uses a sophisticated caching system to dramatically speed up subsequent indexing after the initial index is created.

### How Caching Works

When you index your codebase, codesearch computes **embeddings** (vector representations) for each code chunk. This is the most time-consuming part of indexing. The cache system stores these embeddings so they don't need to be recomputed.

```bash
# First time: slow (all embeddings computed)
codesearch index
# Takes ~2-5 minutes for 10k files (depends on CPU)

# Second time: fast (embeddings loaded from cache)
codesearch index
# Takes ~10-30 seconds (only changed files processed)

# Switching branches: very fast (embeddings reused from cache)
git checkout feature-branch
# Index auto-refreshes in ~5-10 seconds (only new/changed files)
```

### Cache Types

codesearch uses **three cache layers** for optimal performance:

#### 1. In-Memory Cache (Moka LRU Cache)
- **Location**: RAM during indexing process
- **Size**: 100MB (configurable via `CODESEARCH_CACHE_MAX_MEMORY`)
- **Purpose**: Cache embeddings during a single indexing session
- **Benefit**: Avoids recomputing embeddings for duplicate chunks within the same index run

#### 2. Persistent Cache (Disk-Based)
- **Location**: `~/.codesearch/embedding_cache/<model_short_name>/`
- **Size**: Up to 200,000 entries (~300MB)
- **Purpose**: Long-term storage keyed by content hash (SHA256)
- **Benefit**: Embeddings survive MCP restarts and branch switches
- **Key Benefit**: Files with identical content across different branches share the same embedding

#### 3. Query Cache (Optional)
- **Location**: In-memory during search operations
- **Purpose**: Cache query embeddings for repeated searches
- **Benefit**: Repeated searches with the same query are nearly instant

### Cache Benefits

| Scenario | Without Cache | With Cache |
|---|---|---|
| First index (10k files) | ~2-5 min | ~2-5 min (cache empty) |
| Incremental index (1% changed) | ~30 sec | ~10 sec |
| Branch switch (50% overlap) | ~1-2 min | ~10 sec |
| Repeated queries | ~500ms | ~50ms |

### Cache Management

```bash
# Show cache statistics (all models)
codesearch cache stats

# Show cache statistics for specific model
codesearch cache stats bge-small

# Clear persistent cache for specific model
codesearch cache clear bge-small

# Clear cache without confirmation
codesearch cache clear bge-small --yes
```

### Cache Size Monitoring

The persistent cache automatically manages disk usage:
- Default limit: 200,000 entries (~300MB)
- Older entries are evicted when limit is reached (LRU policy)
- Per-model isolation: Each embedding model has its own cache

**Note:** The persistent cache is separate from the index database (`.codesearch.db/`). Clearing the cache does NOT delete your search index — it only deletes cached embeddings, which will be recomputed on the next index.

---

## Searching

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
| `--filter-path` | | | Restrict to path (e.g., `src/api/`) |
| `--vector-only` | | | Disable hybrid, vector similarity only |
| `--rerank` | | | Enable neural reranking (~1.7s extra) |
| `--rerank-top` | | 50 | Candidates to rerank |
| `--rrf-k` | | 20 | RRF fusion parameter |

```bash
codesearch search "database connection pooling"
codesearch search "error handling" --content --rerank
codesearch search "validation" --filter-path src/api --json -m 10
codesearch search "new feature" --sync
```

---

## MCP Server Configuration

The MCP server is codesearch's primary integration point for AI coding agents. It exposes token-efficient tools for semantic code search. The MCP server **auto-detects** the nearest database (local or global) — no project path argument is needed. If no database is found, the server will **not start**. This is intentional: codesearch never creates a database automatically to avoid polluting your projects.

> **Important:** Always `codesearch index` your project first before using the MCP server.

### OpenCode (recommended)

OpenCode is the primary target for codesearch's MCP integration. Add the following to your OpenCode config at `~/.config/opencode/opencode.json`:

```json
{
  "mcp": {
    "codesearch": {
      "type": "local",
      "command": [
        "codesearch",
        "mcp"
      ],
      "enabled": true
    }
  }
}
```

No project path required — codesearch auto-detects the database for the current working directory.

> **⚠️ `codesearch` must be on your system `PATH`** for OpenCode to find it. If you built from source, copy the binary to a directory that's in your `PATH` (e.g., `~/.local/bin/` on Linux/macOS or `C:\Users\<you>\.local\bin\` on Windows). Verify with: `codesearch --version`

### Claude Code

Add to `~/.config/claude-code/config.json`:

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

On Windows, use the full path to `codesearch.exe` if it's not in your `PATH`. Restart Claude Code after editing the config.

### What Happens on Startup

When the MCP server starts, it goes through this sequence:

1. **Database discovery** — Searches for `.codesearch.db/` at the git root (by detecting `.git/` from the current directory), then walks up parent directories (up to 10 levels for non-git projects), and finally checks the global location (`~/.codesearch.dbs/`). The first database found is used. If none is found, the server exits — it will never create a database on its own.
2. **Incremental index** — Automatically runs an incremental re-index against the detected database, so the index is up-to-date before the agent starts working.
3. **File system watcher (FSW)** — Starts watching the project directory for changes. Any file modifications, additions, or deletions are picked up and the index is updated in the background (with debouncing), keeping the database current throughout the session.
4. **Git HEAD watcher** — Monitors `.git/HEAD` for branch changes. When a branch switch is detected, an automatic incremental re-index is triggered to update the database with files from the new branch.

> **Important:** Databases are discovered at the *git repository root*, not in subdirectories. Do not manually create `.codesearch.db/` directories inside subfolders — this will cause confusion. One database per git repository, at the git root (or global).

### MCP Tools

| Tool | Parameters | Description |
|---|---|---|
| `semantic_search` | `query`, `limit`, `compact` (default: true), `filter_path` | Semantic code search. Compact mode returns metadata only (~93% fewer tokens). |
| `find_references` | `symbol`, `limit` (default: 50) | Find all usages/call sites of a symbol across the codebase. |
| `get_file_chunks` | `path`, `compact` (default: true) | Get all indexed chunks from a file. |
| `find_databases` | | Discover available codesearch databases. |
| `index_status` | | Check index existence and statistics. |

### How AI Agents Use the Tools

The MCP tools are designed to work together in a **search → narrow → read** workflow that minimizes token usage:

1. **`semantic_search`** — The agent starts here. A natural language query like `"where do we handle authentication?"` returns a ranked list of matches. With `compact=true` (the default), only metadata is returned: file path, line numbers, chunk kind, signature, and score — roughly 40 tokens per result instead of 600.

2. **`find_references`** — Once the agent identifies a relevant function or symbol, it can ask for all usages and call sites across the codebase. This is much more efficient than grep-based searching and stays within the codesearch ecosystem. Example: `find_references("authenticate")` returns every location that calls or references that symbol.

3. **`get_file_chunks`** — To get a broader view of a specific file's structure, the agent can retrieve all indexed chunks. With `compact=true` this gives an outline (functions, classes, methods with signatures); with `compact=false` it includes full source code.

4. **Targeted file reads** — Finally, the agent reads only the specific lines it needs using its built-in file read tools.

**Example session:**
```
Agent: semantic_search("auth handler", compact=true)
  → 20 results, ~800 tokens total (paths, signatures, scores)

Agent: find_references("authenticate")
  → 8 call sites across 5 files, ~100 tokens

Agent: read("src/auth/handler.rs", lines 45-75)
  → Only the code that matters
```

This workflow typically saves **90%+ tokens** compared to returning full code content for every search result.

---

## Other Commands

| Command | Description |
|---|---|
| `codesearch serve [PATH] -p <PORT>` | HTTP server with live file watching (default port 4444) |
| `codesearch stats [PATH]` | Show database statistics |
| `codesearch clear [PATH] [-y]` | Delete the index |
| `codesearch list` | List all indexed repositories |
| `codesearch doctor` | Check installation health |
| `codesearch setup [--model <MODEL>]` | Pre-download embedding models |

### HTTP Server API

| Method | Endpoint | Description |
|---|---|---|
| GET | `/health` | Health check |
| GET | `/status` | Index statistics |
| POST | `/search` | Search (JSON body: `{"query": "...", "limit": 10}`) |

---

## Search Modes

| Mode | Command | Speed | Best For |
|---|---|---|---|
| **Hybrid** (default) | `codesearch search "query"` | ~75ms | Most queries — balances semantic + keyword |
| **Vector-only** | `codesearch search "query" --vector-only` | ~72ms | Conceptual queries without exact keywords |
| **Hybrid + Reranking** | `codesearch search "query" --rerank` | ~1.8s | Maximum accuracy |

---

## Global vs Local Indexes

codesearch supports two index locations per project. Only one can be active at a time.

| | Local Index | Global Index |
|---|---|---|
| **Location** | `<git-root>/.codesearch.db/` | `~/.codesearch.dbs/<project>/` |
| **Created with** | `codesearch index` (default) | `codesearch index --add -g` |
| **Visible to** | Only when inside the project tree | From any directory |
| **Use case** | Per-project, self-contained | Shared/central index, searchable from anywhere |

**How discovery works:** when you run a command, codesearch looks for a database in this order:
1. `.codesearch.db/` at the git root (automatically detected from current directory)
2. `.codesearch.db/` in parent directories (up to 10 levels, for non-git projects)
3. `~/.codesearch.dbs/` (global)

This means you can `cd` into any subfolder and codesearch will still find the project index at the git root.

### Git Worktrees

codesearch works naturally with [git worktrees](https://git-scm.com/docs/git-worktree). Each worktree lives in its own directory and points to a different branch of the same git repository, so each worktree can have its own independent database and MCP server instance. This means you can have separate indexes for different branches — when OpenCode or Claude Code starts in a worktree folder, codesearch auto-detects the database for that specific worktree.

```bash
# Main repo on main branch
cd /projects/myapp
codesearch index

# Worktree for a feature branch
git worktree add /projects/myapp-feature feature/new-auth
cd /projects/myapp-feature
codesearch index

# Each worktree has its own .codesearch.db/ and MCP instance
# Branch switching within a worktree triggers automatic index refresh
```

```bash
codesearch index                 # Create local index (default)
codesearch index --add -g        # Create global index
codesearch index rm              # Remove whichever index exists
codesearch index list            # Show which index is active
```

---

## Supported Languages

### Full AST Chunking (Tree-sitter)

Rust (`.rs`), Python (`.py`, `.pyw`, `.pyi`), JavaScript (`.js`, `.mjs`, `.cjs`), TypeScript (`.ts`, `.mts`, `.cts`, `.tsx`, `.jsx`), C (`.c`, `.h`), C++ (`.cpp`, `.cc`, `.cxx`, `.hpp`), C# (`.cs`), Go (`.go`), Java (`.java`)

### Line-based Chunking

Ruby, PHP, Swift, Kotlin, Shell, Markdown, JSON, YAML, TOML, SQL, HTML, CSS/SCSS/SASS/LESS

---

## Embedding Models

| Name | ID | Dimensions | Speed | Notes |
|---|---|---|---|---|
| MiniLM-L6 (Q) | `minilm-l6-q` | 384 | Fastest | **Default** |
| MiniLM-L6 | `minilm-l6` | 384 | Fastest | General use |
| MiniLM-L12 (Q) | `minilm-l12-q` | 384 | Fast | Higher quality |
| BGE Small (Q) | `bge-small-q` | 384 | Fast | General use |
| BGE Base | `bge-base` | 768 | Medium | Higher quality |
| BGE Large | `bge-large` | 1024 | Slow | Highest quality |
| **Jina Code** | **`jina-code`** | 768 | Medium | **Code-specific** |
| Nomic v1.5 | `nomic-v1.5` | 768 | Medium | Long context |
| E5 Multilingual | `e5-multilingual` | 384 | Fast | Non-English code |
| MxBai Large | `mxbai-large` | 1024 | Slow | High quality |

The model used for indexing is stored in metadata. Always search with the same model you indexed with, or re-index with `--force` when switching.

---

## Configuration

### Environment Variables

| Variable | Description | Default |
|---|---|---|
| `CODESEARCH_CACHE_MAX_MEMORY` | Max embedding cache in MB | 500 |
| `CODESEARCH_BATCH_SIZE` | Embedding batch size | Auto |
| `RUST_LOG` | Logging level | `codesearch=info` |

### Ignore Files

Create `.codesearchignore` in your project root (same syntax as `.gitignore`). Also respects `.gitignore` and `.osgrepignore`.

### Global Options

| Option | Short | Description |
|---|---|---|
| `--loglevel` | | Set log level (error, warn, info, debug, trace) |
| `--quiet` | `-q` | Suppress info, only results/errors |
| `--model` | | Override embedding model |
| `--store` | | Override store name |

---

## How It Works

1. **File Discovery** — Walks the directory respecting ignore files, detects language, skips binaries.
2. **Git Root Detection** — Automatically finds the git repository root and places `.codesearch.db/` there, ensuring a single index per repository.
3. **Semantic Chunking** — Tree-sitter AST parsing extracts functions, classes, methods with metadata. Falls back to line-based chunking for unsupported languages.
4. **Embedding Generation** — fastembed + ONNX Runtime (CPU), batched, with SHA-256 change detection and **caching**.
5. **Vector Storage** — arroy (ANN search) + LMDB (ACID persistence) in a single `.codesearch.db/` directory at git root.
6. **Incremental Updates** — FileMetaStore tracks hash/mtime/size; only changed files are re-processed.
7. **Git Branch Detection** — Monitors `.git/HEAD` for branch switches and automatically refreshes the index.
8. **Search** — Query → embed → vector search → BM25 → RRF fusion → (optional) reranking.

---

## Troubleshooting

| Problem | Solution |
|---|---|
| "No database found" | Run `codesearch index` first (creates index at git root) |
| Poor search results | Try `--sync` to update, `--rerank` for accuracy, or `--force` to rebuild |
| Model mismatch warning | Re-index: `codesearch index --force --model <model>` |
| Out of memory | `CODESEARCH_BATCH_SIZE=32 codesearch index` |
| Port in use (serve) | `codesearch serve --port 5555` |
| Wrong database found | Check where `.codesearch.db/` is located with `codesearch list` |
| Index not updating after branch switch | The Git HEAD watcher refreshes automatically; check `codesearch stats` to verify |
| First index very slow | Normal! First time indexes compute all embeddings (2-5 min). Subsequent indexes use cache (10-30 sec) |
| Cache too large | Clear cache: `codesearch cache clear <model>` |

### Git-Specific Troubleshooting

**"Multiple .git directories detected"**
- This error occurs when codesearch finds nested git repositories
- Solution: Remove the nested `.git` directory or index from the outer repository only

**"Database not at git root"**
- Old versions of codesearch created databases in the current directory
- Solution: Delete the old `.codesearch.db/` directory and run `codesearch index` — it will be recreated at the git root

### Debug Logging

```bash
RUST_LOG=codesearch=debug codesearch search "query"
RUST_LOG=codesearch::embed=trace codesearch index
```

---

## Development

```bash
cargo build              # Debug
cargo build --release    # Release
cargo test               # Tests
cargo fmt                # Format
cargo clippy             # Lint
```

---

## License

Apache-2.0

## Acknowledgements

This project is a fork of [demongrep](https://github.com/yxanul/demongrep) by [yxanul](https://github.com/yxanul). A huge thank you for building such a solid and well-designed foundation — without demongrep, codesearch wouldn't exist.
