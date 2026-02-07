# codesearch

**Fast, local semantic code search powered by Rust.**

Search your codebase using natural language queries like *"where do we handle authentication?"* — all running locally with no API calls.

## Features

- **Semantic Search** — Natural language queries that understand code meaning
- **Hybrid Search** — Combines vector similarity + BM25 full-text search with RRF fusion
- **Neural Reranking** — Optional second-pass reranking with Jina Reranker for higher accuracy
- **Smart Chunking** — Tree-sitter AST-aware chunking that preserves functions, classes, methods
- **Context Windows** — Shows surrounding code (3 lines before/after) for better understanding
- **Incremental Indexing** — Only re-indexes changed files for 10-100x faster updates
- **Database Discovery** — Automatically finds databases in parent/global directories
- **Local & Private** — All processing happens locally using ONNX models, no data leaves your machine
- **Fast** — Sub-second search after initial model load
- **Multiple Interfaces** — CLI, HTTP server, and MCP server for Claude Code integration
- **AI Agent Friendly** — Smart grep wrapper for OpenAgents/OpenCoder integration

---

## Table of Contents

- [Installation](#installation)
- [Quick Start](#quick-start)
- [What's New](#whats-new)
- [Command Reference](#command-reference)
  - [search](#search)
  - [index](#index)
  - [serve](#serve)
  - [mcp](#mcp)
  - [stats](#stats)
  - [clear](#clear)
  - [list](#list)
  - [doctor](#doctor)
  - [setup](#setup)
- [Global Options](#global-options)
- [Search Modes](#search-modes)
- [MCP Server (Claude Code)](#mcp-server-claude-code-integration)
- [HTTP Server API](#http-server-api)
- [Database Management](#database-management)
- [AI Agent Integration](#ai-agent-integration)
- [Supported Languages](#supported-languages)
- [Embedding Models](#embedding-models)
- [Configuration](#configuration)
- [How It Works](#how-it-works)
- [Troubleshooting](#troubleshooting)

---

## Installation

### Prerequisites

#### Linux (Ubuntu/Debian)
```bash
sudo apt-get update
sudo apt-get install -y build-essential protobuf-compiler libssl-dev pkg-config
```

#### Linux (Fedora/RHEL)
```bash
sudo dnf install -y gcc protobuf-compiler openssl-devel pkg-config
```

#### macOS
```bash
brew install protobuf openssl pkg-config
```

#### Windows
```powershell
# Using winget
winget install -e --id Google.Protobuf

# Or using chocolatey
choco install protoc
```

### Building from Source

```bash
# Clone the repository
git clone https://github.com/flupkede/codesearch.git
cd codesearch

# Build release binary
cargo build --release

# The binary is at target/release/codesearch
# Optionally, copy to your PATH:
sudo cp target/release/codesearch /usr/local/bin/
```

### Verify Installation

```bash
codesearch --version
codesearch doctor  # Check system health
```

---

## Quick Start

```bash
# 1. Navigate to your project
cd /path/to/your/project

# 2. Index the codebase (first time only, ~30-60s for medium projects)
codesearch index

# 3. Search with natural language
codesearch search "where do we handle authentication?"

# 4. Search with better accuracy (slower)
codesearch search "error handling" --rerank
```

---

## What's New

### Version 0.2.1 (2025-02-07)

#### 🔍 MCP Server Improvements
- **Token-efficient search** — `compact` mode (default: true) returns only metadata (path, line numbers, kind, signature, score) instead of full content
- **Token savings** — ~93% reduction in semantic search tokens (12k → 800 tokens for 20 results)
- **New `find_references` tool** — Locate symbol usages/call sites across codebase without grep
- **New `filter_path` parameter** — Restrict search to specific directories (e.g., `src/api/`)
- **Compact JSON output** — ~30% less whitespace tokens in all MCP responses
- **Token-efficient workflow** — Model guidance: search → find_references → targeted reads

#### Usage Examples

```bash
# Build release binary
cargo build --release

# Start MCP server
./target/release/codesearch mcp .

# Claude Code can now:
# - semantic_search("auth handler", compact=true)  → metadata only (~800 tokens)
# - find_references("authenticate")                 → all call sites (~100 tokens)
# - read(path, offset, limit)                      → specific code only
```

### Version 0.2.0 (2025-01-22)

#### 🚀 Incremental Indexing
- `codesearch index` now automatically performs incremental updates when database exists
- Only re-indexes changed, added, and deleted files
- Uses FileMetaStore to track file metadata (hash, mtime, size)
- Early exit if database is already up-to-date
- **10-100x faster** updates for typical workflows

#### 🔍 Database Discovery
- Index command now searches parent/global directories for existing databases
- Works from any subfolder - automatically finds the project database
- Consistent behavior with search command
- Shows informative message when using database from parent directory

#### 🎯 CLI Improvements
- Added `--full` and `-f` as aliases for `--force` flag
- Better user feedback during incremental indexing
- Clear progress indicators for changed/unchanged/deleted files

#### 🤖 AI Agent Integration
- Smart grep wrapper for OpenAgents, OpenCoder, and TechnicalWriter
- Automatically uses codesearch for indexed source code projects
- Falls back to regular grep for non-code files
- Optimized for ASP.NET Core (`.cs`, `.cshtml`, `.razor`, `.csproj`, `.sln`, `.sql`)
- Minimal performance overhead

#### Usage Examples

```bash
# Incremental index (default when DB exists)
codesearch index

# Full re-index
codesearch index --force
codesearch index --full
codesearch index -f

# Index from subfolder (finds parent database)
cd src/components
codesearch index
```

---

## Command Reference

### search

Search the codebase using natural language queries.

```bash
codesearch search <QUERY> [OPTIONS]
```

#### Arguments

| Argument | Description |
|----------|-------------|
| `<QUERY>` | Natural language search query (e.g., "where do we handle authentication?") |

#### Options

| Option | Short | Default | Description |
|--------|-------|---------|-------------|
| `--max-results` | `-m` | 25 | Maximum total results to return |
| `--per-file` | | 1 | Maximum matches to show per file |
| `--content` | `-c` | | Show full chunk content instead of snippets |
| `--scores` | | | Show relevance scores and timing information |
| `--compact` | | | Show file paths only (like `grep -l`) |
| `--sync` | `-s` | | Re-index changed files before searching |
| `--json` | | | Output results as JSON (for scripting/agents) |
| `--path` | | `.` | Path to search in |
| `--filter-path` | | | Only show results from files under this path (e.g., `src/`) |
| `--vector-only` | | | Disable hybrid search, use vector similarity only |
| `--rerank` | | | Enable neural reranking for better accuracy (~1.7s extra) |
| `--rerank-top` | | 50 | Number of candidates to rerank |
| `--rrf-k` | | 20 | RRF fusion parameter (higher = more weight to rank position) |

#### Examples

```bash
# Basic search
codesearch search "database connection pooling"

# Show full code content with context
codesearch search "error handling" --content

# Get JSON output for scripting
codesearch search "authentication" --json -m 10

# Search only in src/api directory
codesearch search "validation" --filter-path src/api

# High-accuracy search with reranking
codesearch search "complex algorithm" --rerank

# Quick search with scores
codesearch search "config loading" --scores

# Re-index changed files, then search
codesearch search "new feature" --sync

# File paths only
codesearch search "tests" --compact
```

---

### index

Index a codebase for semantic search.

```bash
codesearch index [PATH] [OPTIONS]
```

#### Arguments

| Argument | Description |
|----------|-------------|
| `[PATH]` | Path to index (defaults to current directory), or use "list" to show status |

#### Options

| Option | Short | Description |
|--------|-------|-------------|
| `--dry-run` | | Preview what would be indexed without indexing |
| `--force` | `-f` | Delete existing index and rebuild from scratch (also aliased as `--full`) |
| `--add` | | Create a new index (local or global with `-g`) |
| `--global` | `-g` | Create global index instead of local (only with `--add`) |
| `--rm` | | Remove the index (auto-detects local or global, alias: `--remove`) |
| `--list` | | Show index status (local or global) |
| `--model` | | Override embedding model |

#### Examples

```bash
# Index current directory (auto-detects local or global)
codesearch index

# Index a specific project
codesearch index /path/to/project

# Force complete re-index (delete and rebuild)
codesearch index --force
codesearch index --full
codesearch index -f

# Create new index
codesearch index add                # Create local index
codesearch index add -g             # Create global index

# Remove index (auto-detects which to remove)
codesearch index rm
codesearch index remove

# Show index status
codesearch index list
codesearch index --list

# Preview files to be indexed
codesearch index --dry-run

# Index with a specific model
codesearch index --model jina-code
```

#### What Gets Indexed

- All text files respecting `.gitignore`
- Custom ignore patterns from `.codesearchignore` or `.osgrepignore`
- Skips binary files, `node_modules/`, `.git/`, etc.

#### Index Location

The index is stored in one of two locations:
- **Local**: `.codesearch.db/` directory inside your project root
- **Global**: `~/.codesearch.dbs/` (searchable from any location)

Only one index type can exist per project (local OR global, never both).

#### Incremental Indexing

When the database exists, `codesearch index` automatically performs incremental updates:

```
📊 Incremental Indexing
------------------------------------------------------------
   Unchanged files: 142
   Changed files: 3
   Deleted files: 1

🔄 Deleting 4 old chunks...
✅ Deleted 4 chunks

🔄 Processing 3 changed files...
✅ Indexed 3 files
```

#### Index Status

Check which index exists and view statistics:

```bash
codesearch index list
```

Output:
```
📋 Index Status
============================================================

💾 Database: /path/to/project/.codesearch.db
   Type: Local
   Status: ✅ Indexed
   Chunks: 873
   Size: 8.77 MB
```

---

### serve

Run an HTTP server with live file watching for continuous indexing.

```bash
codesearch serve [PATH] [OPTIONS]
```

#### Options

| Option | Short | Default | Description |
|--------|-------|---------|-------------|
| `--port` | `-p` | 4444 | Port to listen on |

#### Examples

```bash
# Start server on default port (4444)
codesearch serve

# Start server on custom port
codesearch serve --port 3333

# Serve a specific project
codesearch serve /path/to/project --port 8080
```

The server automatically re-indexes files when they change (with 300ms debouncing).

---

### mcp

Start an MCP (Model Context Protocol) server for Claude Code integration.

```bash
codesearch mcp [PATH]
```

#### Arguments

| Argument | Description |
|----------|-------------|
| `[PATH]` | Path to project (defaults to current directory) |

See [MCP Server section](#mcp-server-claude-code-integration) for detailed setup.

---

### stats

Show statistics about the indexed database.

```bash
codesearch stats [PATH]
```

#### Output

```
📊 Database Statistics
============================================================
💾 Database: /path/to/project/.codesearch.db
📂 Project: /path/to/project

Vector Store:
   Total chunks: 731
   Total files: 45
   Indexed: ✅ Yes
   Dimensions: 384

Storage:
   Database size: 12.34 MB
   Avg per chunk: 17.28 KB
```

---

### clear

Delete the index database.

```bash
codesearch clear [PATH] [OPTIONS]
```

#### Options

| Option | Short | Description |
|--------|-------|-------------|
| `--yes` | `-y` | Skip confirmation prompt |

#### Examples

```bash
# Clear with confirmation
codesearch clear

# Clear without confirmation (for scripts)
codesearch clear -y

# Clear a specific project's index
codesearch clear /path/to/project -y
```

---

### list

List all indexed repositories (searches for `.codesearch.db` directories).

```bash
codesearch list
```

---

### doctor

Check installation health and system requirements.

```bash
codesearch doctor
```

---

### setup

Pre-download embedding models.

```bash
codesearch setup [OPTIONS]
```

#### Options

| Option | Description |
|--------|-------------|
| `--model` | Specific model to download (defaults to default model) |

---

## Global Options

These options work with all commands:

| Option | Short | Description |
|--------|-------|-------------|
| `--verbose` | `-v` | Enable verbose/debug output |
| `--quiet` | `-q` | Suppress informational output (only results/errors) |
| `--model` | | Override embedding model |
| `--store` | | Override store name |
| `--help` | `-h` | Show help |
| `--version` | `-V` | Show version |

---

## Search Modes

codesearch supports three search modes with different accuracy/speed tradeoffs:

### 1. Hybrid Search (Default)

Combines vector similarity with BM25 full-text search using Reciprocal Rank Fusion (RRF).

```bash
codesearch search "query"
```

- **Speed**: ~75ms
- **Best for**: Most queries, balances semantic understanding with keyword matching

### 2. Vector-Only Search

Pure semantic similarity search using embeddings.

```bash
codesearch search "query" --vector-only
```

- **Speed**: ~72ms
- **Best for**: Conceptual queries where exact keywords don't matter

### 3. Hybrid + Neural Reranking

Two-stage search: hybrid retrieval followed by cross-encoder reranking.

```bash
codesearch search "query" --rerank
```

- **Speed**: ~1.8s (adds ~1.7s for reranking)
- **Best for**: When accuracy matters more than speed

---

## MCP Server (Claude Code Integration)

codesearch can act as an MCP server, allowing Claude Code to search your codebase.

### Setup

1. **Build codesearch** and note the binary path:
   ```bash
   cargo build --release
   # Binary at: /path/to/codesearch/target/release/codesearch
   ```

2. **Index your project**:
   ```bash
   cd /path/to/your/project
   codesearch index
   ```

3. **Configure Claude Code**

   Edit `~/.config/claude-code/config.json` (Linux/Mac) or the appropriate config location:

   ```json
   {
     "mcpServers": {
       "codesearch": {
         "command": "/absolute/path/to/codesearch",
         "args": ["mcp", "/absolute/path/to/your/project"]
       }
     }
   }
   ```

4. **Restart Claude Code**

### Available MCP Tools

| Tool | Parameters | Description |
|------|------------|-------------|
| `semantic_search` | `query`, `limit`, `compact` (default: true), `filter_path` | Search code semantically. Returns minimal metadata by default to save tokens. |
| `find_references` | `symbol`, `limit` (default: 50) | Find all usages/call sites of a symbol (function, class, method) across codebase. |
| `get_file_chunks` | `path`, `compact` (default: true) | Get all indexed chunks from a file. Returns metadata by default. |
| `find_databases` | | Find all available codesearch databases in current/parent/global directories. |
| `index_status` | | Check if index exists and get statistics. |

### Token-Efficient Workflow for AI Models

The MCP server is designed to minimize token usage while maintaining full context awareness:

1. **`semantic_search(compact=true)`** — Get metadata-only results (~40 tokens vs 600 tokens per result)
2. **`find_references(symbol)`** — Find all call sites without grep (avoids leaving codesearch ecosystem)
3. **Targeted `read()` calls** — Read only the specific code you need

**Example workflow:**
```
1. semantic_search("auth handler", compact=true)
   → Returns: path, line numbers, kind, signature, score for 20 results (~800 tokens total)

2. find_references("authenticate")
   → Returns: file paths + line numbers for all call sites (~100 tokens)

3. read("src/auth/handler.rs", offset=45, limit=30)
   → Read only the specific code you're analyzing
```

**Token savings:**
- Compact mode reduces semantic search tokens by **~93%** (12k → 800 tokens for 20 results)
- Compact JSON output eliminates **~30% whitespace** tokens
- `find_references` eliminates need for grep search in AI workflows

**When to use `compact=false`:**
- When you need full code content immediately (rare for AI models)
- For quick manual inspection
- When search space is already small (e.g., single file)

### Example MCP Usage in Claude Code

Once configured, Claude Code can use commands like:
- *"Search for authentication handling in src/api/"* — Uses `semantic_search` with `filter_path="src/api/"`
- *"Find all references to the authenticate function"* — Uses `find_references("authenticate")`
- *"Find all chunks in src/auth.rs"* — Uses `get_file_chunks("src/auth.rs", compact=false)`
- *"Check if the index is ready"* — Uses `index_status`

---

## HTTP Server API

### Endpoints

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/health` | Health check (returns `{"status": "ok"}`) |
| GET | `/status` | Index statistics |
| POST | `/search` | Search the codebase |

### Search API

**Request:**
```bash
curl -X POST http://localhost:4444/search \
  -H "Content-Type: application/json" \
  -d '{
    "query": "authentication",
    "limit": 10
  }'
```

**Response:**
```json
{
  "results": [
    {
      "path": "src/auth/handler.rs",
      "start_line": 45,
      "end_line": 67,
      "kind": "Function",
      "content": "pub fn authenticate(...) { ... }",
      "score": 0.89,
      "signature": "fn authenticate(credentials: &Credentials) -> Result<User>"
    }
  ],
  "query": "authentication",
  "total_results": 1
}
```

---

## Database Management

### Index Location

Each project has its own index at `<project_root>/.codesearch.db/`

### Database Discovery

codesearch automatically searches for databases in:
1. Current directory: `.codesearch.db/`
2. Parent directories (up to 10 levels up)
3. Global location: `~/.codesearch.dbs/` (if configured)

This means you can run commands from any subfolder and codesearch will find the project database.

### Re-indexing

```bash
# Incremental update (only changed files) - DEFAULT
codesearch index

# Or with search
codesearch search "query" --sync

# Full rebuild (delete and recreate)
codesearch index --force
```

### Delete Index

```bash
# Interactive
codesearch clear

# Non-interactive
codesearch clear -y

# Or manually
rm -rf .codesearch.db/
```

### Check Index Status

```bash
codesearch stats
```

---

## AI Agent Integration

codesearch includes a smart grep wrapper that allows AI agents (OpenAgents, OpenCoder, TechnicalWriter) to automatically use codesearch for indexed source code projects.

### How It Works

The wrapper at `~/.local/bin/grep` automatically:
1. Detects if you're in a source code project
2. Checks if a `.codesearch.db` exists
3. Uses `codesearch` for indexed projects
4. Falls back to regular `grep` for non-code or non-indexed projects

### Supported File Types

Optimized for ASP.NET Core and web development:
- **C#/.NET**: `.cs`, `.cshtml`, `.razor`, `.csproj`, `.sln`, `.sql`
- **Web**: `.ts`, `.tsx`, `.js`, `.jsx`, `.vue`, `.svelte`
- **Other**: `.rs`, `.go`, `.py`, `.java`, `.c`, `.cpp`, etc.

### Performance

- Minimal overhead (< 10ms) for project detection
- No performance impact in non-code directories
- Fast fallback to regular grep when needed

### Setup

The wrapper is automatically installed at `~/.local/bin/grep` and added to your PATH in `~/.bashrc`.

---

## Supported Languages

### Full Semantic Chunking (Tree-sitter AST)

These languages have full AST-aware chunking that extracts functions, classes, methods, etc.:

| Language | Extensions |
|----------|------------|
| Rust | `.rs` |
| Python | `.py`, `.pyw`, `.pyi` |
| JavaScript | `.js`, `.mjs`, `.cjs` |
| TypeScript | `.ts`, `.mts`, `.cts`, `.tsx`, `.jsx` |

### Indexed (Line-based Chunking)

These languages are indexed with fallback line-based chunking:

| Language | Extensions |
|----------|------------|
| Go | `.go` |
| Java | `.java` |
| C | `.c`, `.h` |
| C++ | `.cpp`, `.cc`, `.cxx`, `.hpp` |
| **C#** | **`.cs`** |
| Ruby | `.rb`, `.rake` |
| PHP | `.php` |
| Swift | `.swift` |
| Kotlin | `.kt`, `.kts` |
| Shell | `.sh`, `.bash`, `.zsh` |
| Markdown | `.md`, `.markdown`, `.txt` |
| JSON | `.json` |
| YAML | `.yaml`, `.yml` |
| TOML | `.toml` |
| **SQL** | **`.sql`** |
| HTML | `.html`, `.htm` |
| CSS | `.css`, `.scss`, `.sass`, `.less` |

---

## Embedding Models

### Available Models

| Name | ID | Dimensions | Speed | Quality | Best For |
|------|-----|------------|-------|---------|----------|
| MiniLM-L6 | `minilm-l6` | 384 | Fastest | Excellent | General use |
| MiniLM-L6 (Q) | `minilm-l6-q` | 384 | Fastest | Excellent | **Default** |
| MiniLM-L12 | `minilm-l12` | 384 | Fast | Better | Higher quality |
| MiniLM-L12 (Q) | `minilm-l12-q` | 384 | Fast | Better | Higher quality |
| BGE Small | `bge-small` | 384 | Fast | Good | General use |
| BGE Small (Q) | `bge-small-q` | 384 | Fast | Good | General use |
| BGE Base | `bge-base` | 768 | Medium | Better | Higher quality |
| BGE Large | `bge-large` | 1024 | Slow | Best | Highest quality |
| **Jina Code** | **`jina-code`** | 768 | Medium | Excellent | **Code-specific** |
| Nomic v1.5 | `nomic-v1.5` | 768 | Medium | Good | Long context |
| E5 Multilingual | `e5-multilingual` | 384 | Fast | Good | Non-English code |
| MxBai Large | `mxbai-large` | 1024 | Slow | Excellent | High quality |

### Changing Models

```bash
# Index with specific model
codesearch index --model jina-code

# Search must use same model as index
codesearch search "query" --model jina-code
```

**Note:** The model used for indexing is saved in metadata. If you search with a different model, you may get poor results. Use `--force` to re-index with a new model.

---

## Configuration

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `CODESEARCH_CACHE_MAX_MEMORY` | Maximum memory for embedding cache in MB | 500 |
| `DEMONGREP_BATCH_SIZE` | Embedding batch size | Auto (based on model) |
| `RUST_LOG` | Logging level | `codesearch=info` |

### Memory Optimization

**Before:** Cache grew unbounded → 1GB+ RAM during indexing

**After:** Automatic memory management with Moka cache
- Default: 500 MB max cache memory
- Automatic LRU eviction when limit reached
- Typical usage: 100-150 MB idle, 300-600 MB during indexing

**Customize cache size:**
```bash
# Use 200 MB cache
CODESEARCH_CACHE_MAX_MEMORY=200 codesearch index

# Use 1 GB cache (for large codebases)
CODESEARCH_CACHE_MAX_MEMORY=1024 codesearch index
```

### Ignore Files

Create `.codesearchignore` in your project root:

```gitignore
# Ignore test fixtures
**/fixtures/**
**/testdata/**

# Ignore generated code
**/generated/**
*.generated.ts

# Ignore large files
*.min.js
*.bundle.js
```

codesearch also respects `.gitignore` and `.osgrepignore` files.

---

## How It Works

### 1. File Discovery
- Walks directory respecting `.gitignore` and custom ignore files
- Detects language from file extensions
- Skips binary files automatically

### 2. Semantic Chunking
- Parses code with tree-sitter (native Rust implementation)
- Extracts semantic units: functions, classes, methods, structs, traits, impls
- Preserves metadata: signatures, docstrings, context breadcrumbs
- Falls back to line-based chunking for unsupported languages

### 3. Embedding Generation
- Uses fastembed with ONNX Runtime (CPU-optimized)
- Batched processing for efficiency
- SHA-256 content hashing for change detection

### 4. Vector Storage
- arroy for approximate nearest neighbor search
- LMDB for ACID transactions and persistence
- Single `.codesearch.db/` directory per project

### 5. Incremental Indexing (NEW)
- Tracks file metadata (hash, mtime, size) in FileMetaStore
- Only re-indexes changed files
- Detects and removes deleted files
- 10-100x faster than full re-index

### 6. Search
- Query embedding → Vector search → BM25 search → RRF fusion → (Optional) Reranking

---

## Troubleshooting

### "No database found"

```bash
# Index the project first
codesearch index
```

### Search returns poor results

1. **Check if index is stale:**
   ```bash
   codesearch search "query" --sync
   ```

2. **Try different search mode:**
   ```bash
   codesearch search "query" --rerank
   ```

3. **Rebuild index:**
   ```bash
   codesearch index --force
   ```

### Model mismatch warning

If you indexed with one model and search with another:
```bash
# Re-index with the model you want to use
codesearch index --force --model minilm-l6-q
```

### Out of memory during indexing

```bash
# Reduce batch size
DEMONGREP_BATCH_SIZE=32 codesearch index
```

### Server won't start (port in use)

```bash
# Use a different port
codesearch serve --port 5555
```

---

## Development

```bash
# Debug build
cargo build

# Release build
cargo build --release

# Run tests
cargo test

# Format code
cargo fmt

# Lint
cargo clippy
```

### Debug Logging

```bash
RUST_LOG=codesearch=debug codesearch search "query"
RUST_LOG=codesearch::embed=trace codesearch index
```

---

## License

Apache-2.0

---

## Contributing

Contributions welcome! See [AGENTS.md](AGENTS.md) for the changelog.

---

## See Also

- [AGENTS.md](AGENTS.md) - Changelog for AI agents
- [Technical Documentation](.docs/) - Detailed technical docs
