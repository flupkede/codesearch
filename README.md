# codesearch

**Fast, local semantic code search powered by Rust.**

Search your codebase using natural language queries like *"where do we handle authentication?"* — all running locally with no API calls.

> **Fork notice:** This project is a fork of [demongrep](https://github.com/yxanul/demongrep) by [yxanul](https://github.com/yxanul). Huge thanks to yxanul for creating the original project — it's an excellent piece of work and the foundation everything here builds on. Some features (like global database support) were contributed back to demongrep via PR. codesearch extends it further with incremental indexing, MCP token optimizations, AI agent integration, and more.

---

## Features

- **Semantic Search** — Natural language queries that understand code meaning
- **Hybrid Search** — Vector similarity + BM25 full-text search with RRF fusion
- **Neural Reranking** — Optional cross-encoder reranking for higher accuracy
- **Smart Chunking** — Tree-sitter AST-aware chunking that preserves functions, classes, methods
- **Incremental Indexing** — Only re-indexes changed files (10–100× faster updates)
- **Global & Local Indexes** — Per-project local indexes or a shared global index
- **MCP Server** — Token-efficient integration with OpenCode, Claude Code, and any MCP-compatible agent
- **Local & Private** — All processing via ONNX models, no data leaves your machine
- **Fast** — Sub-second search after initial model load

---

## Table of Contents

- [Installation](#installation)
- [Quick Start](#quick-start)
- [Indexing](#indexing)
- [Searching](#searching)
- [MCP Server (OpenCode / Claude Code)](#mcp-server-opencode--claude-code)
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

### Prerequisites

| Platform | Command |
|---|---|
| **Ubuntu/Debian** | `sudo apt-get install -y build-essential protobuf-compiler libssl-dev pkg-config` |
| **Fedora/RHEL** | `sudo dnf install -y gcc protobuf-compiler openssl-devel pkg-config` |
| **macOS** | `brew install protobuf openssl pkg-config` |
| **Windows** | `winget install -e --id Google.Protobuf` or `choco install protoc` |

### Pre-built Binaries

Download the latest release for your platform from [Releases](https://github.com/flupkede/codesearch/releases):

| Platform | Download |
|---|---|
| **Windows x86_64** | `codesearch-windows-x86_64.zip` |
| **Linux x86_64** | `codesearch-linux-x86_64.tar.gz` |
| **macOS (Apple Silicon)** | `codesearch-macos-arm64.tar.gz` |
| **macOS (Intel)** | `codesearch-macos-x86_64.tar.gz` |

Extract and place the binary somewhere on your `PATH`.

### Building from Source

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

## Quick Start

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

## MCP Server (OpenCode / Claude Code)

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
        "--verbose",
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

1. **Database discovery** — Searches for a `.codesearch.db/` in the current directory, then walks up parent directories (up to 10 levels), and finally checks the global location (`~/.codesearch.dbs/`). The first database found is used. If none is found, the server exits — it will never create a database on its own.
2. **Incremental index** — Automatically runs an incremental re-index against the detected database, so the index is up-to-date before the agent starts working.
3. **File system watcher (FSW)** — Starts watching the project directory for changes. Any file modifications, additions, or deletions are picked up and the index is updated in the background (with debouncing), keeping the database current throughout the session.

> **Important:** Databases are discovered in *parent* folders only. Do not place `.codesearch.db/` directories inside subfolders of an already-indexed project — this will cause confusion. One database per project, at the project root (or global).

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
| **Location** | `<project>/.codesearch.db/` | `~/.codesearch.dbs/<project>/` |
| **Created with** | `codesearch index` (default) | `codesearch index --add -g` |
| **Visible to** | Only when inside the project tree | From any directory |
| **Use case** | Per-project, self-contained | Shared/central index, searchable from anywhere |

**How discovery works:** when you run a command, codesearch looks for a database in this order:
1. `.codesearch.db/` in the current directory
2. `.codesearch.db/` in parent directories (up to 10 levels)
3. `~/.codesearch.dbs/` (global)

This means you can `cd` into any subfolder and codesearch will still find the project index.

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
| `--verbose` | `-v` | Debug output |
| `--quiet` | `-q` | Suppress info, only results/errors |
| `--model` | | Override embedding model |
| `--store` | | Override store name |

---

## How It Works

1. **File Discovery** — Walks the directory respecting ignore files, detects language, skips binaries.
2. **Semantic Chunking** — Tree-sitter AST parsing extracts functions, classes, methods with metadata. Falls back to line-based chunking for unsupported languages.
3. **Embedding Generation** — fastembed + ONNX Runtime (CPU), batched, with SHA-256 change detection.
4. **Vector Storage** — arroy (ANN search) + LMDB (ACID persistence) in a single `.codesearch.db/` directory.
5. **Incremental Updates** — FileMetaStore tracks hash/mtime/size; only changed files are re-processed.
6. **Search** — Query → embed → vector search → BM25 → RRF fusion → (optional) reranking.

---

## Troubleshooting

| Problem | Solution |
|---|---|
| "No database found" | Run `codesearch index` first |
| Poor search results | Try `--sync` to update, `--rerank` for accuracy, or `--force` to rebuild |
| Model mismatch warning | Re-index: `codesearch index --force --model <model>` |
| Out of memory | `CODESEARCH_BATCH_SIZE=32 codesearch index` |
| Port in use (serve) | `codesearch serve --port 5555` |

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
