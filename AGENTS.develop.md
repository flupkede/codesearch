# AGENTS.develop.md

This file is the **develop-branch reference** for all coding agents.
It is committed on `develop` and merged from every feature branch.

## Setup (once per machine)

```bash
git config core.hooksPath .githooks
```

This enables the `post-checkout` hook that automatically copies `AGENTS.develop.md`
to `AGENTS.md` whenever you switch to or create a branch where `AGENTS.md` does not
yet exist. After that, add your feature plan to `AGENTS.md` — it stays in the branch
and is never merged back to develop.

## For agents starting a new feature branch

If `AGENTS.md` was not auto-created by the hook, create it manually:

```bash
cp AGENTS.develop.md AGENTS.md
```

Then replace the content under `## Plan` at the bottom with the work plan for this branch.
Leave the architecture sections intact — they provide context.

**At the end of the branch, before the PR:**
- Update the `## Active feature branches` section below (remove this branch)
- Add one line to `## Changelog highlights`
- Commit: `docs: update AGENTS.develop.md for features/xxx`

**No active work plan lives here.** Feature branches carry their own `AGENTS.md`.
This file contains only architecture, conventions, and changelog.

---

## What codesearch is

A fast, local, offline MCP server for semantic code search. Single Rust binary.
No Docker, no cloud, no external services. Designed for coding agents (OpenCode,
Claude Code, Claude Desktop) that need to search and navigate large codebases efficiently.

Core stack: Tantivy (BM25 FTS) + arroy (HNSW vectors) + fastembed/ONNX (embeddings) +
tree-sitter (AST chunking) + LMDB (persistent storage) + rmcp 1.5.0 (MCP protocol).
Hybrid BM25 + vector search fused via RRF.

---

## MCP tools

Five tools exposed to agents:

| Tool | Description |
|---|---|
| `search` | Hybrid semantic + BM25 search. Requires `project` or `group` in multi-repo mode. |
| `find` | Symbol navigation: definition, usages, imports, dependents. Requires scope. |
| `explore` | File outline or similar-chunk lookup. Requires scope. |
| `get_chunk` | Retrieve a chunk by ID with optional context lines. Requires `project` in multi-repo mode. |
| `status` | Index and project status. Lightweight (no DB open) when called without scope. |

All tools return `scope_required` structured errors in multi-repo mode when no `project`
or `group` is specified, with `available_projects`, `available_groups`, and `hint_for_agent`.

---

## Multi-repo serve mode

`codesearch serve` starts an MCP HTTP server on `127.0.0.1:{port}` (default 39725).
Multiple repos register via `repos.json` with aliases. Agents route queries per-alias
or per-group.

**Repo lifecycle:**
- `Warm` — DB open, vector index ready, no FSW. State after background warmup or fan-out open.
- `Write` — Warm + file system watcher running. Transitions from Warm on first explicit project query.
- `Readonly` — Another process holds the write lock.
- `Closed` — Evicted by idle reaper after `REPO_IDLE_TIMEOUT_SECS` (30 min default) of inactivity.

**Idle reaper** runs every `REAPER_INTERVAL_SECS` (5 min). Evicts repos not queried within timeout.
All opens update `last_access` so the reaper can track every open repo.

**Fan-out rule:** `get_chunk` and group queries use `touch=false` → repos open as Warm only,
no FSW spawned. Only explicit `project=` queries use `touch=true` → Warm→Write transition.

---

## TUI

`codesearch serve` with a TTY starts an embedded ratatui TUI (repo table, status, CPU).
Without TTY: headless, logs only.

`codesearch serve --no-tui` — suppress TUI even with TTY (e.g. to run serve in one terminal
and open the TUI separately in another).

`codesearch serve tui [--url http://...]` — standalone TUI that connects to a running serve
instance via HTTP polling of the `GET /status` endpoint. Can be opened and closed independently.

---

## HTTP endpoints (serve mode)

| Endpoint | Method | Description |
|---|---|---|
| `/health` | GET | Health check JSON |
| `/status` | GET | Lightweight repo state snapshot for TUI polling |
| `/repos` | POST | Register + index + warmup a new repo |
| `/repos/:alias` | DELETE | Stop FSW, evict, unregister, delete DB |
| `/repos/:alias/reindex` | POST | Incremental or force reindex (background) |
| `/mcp` | GET/POST | MCP streamable HTTP endpoint |

---

## Supported languages (tree-sitter AST chunking)

Rust, Python, JavaScript, TypeScript, C, C++, C#, Go, Java (9 languages).

---

## Release artifacts

GitHub Actions produces 3 release binaries per tag:

```
codesearch-windows-x86_64.zip
codesearch-linux-x86_64.tar.gz
codesearch-macos-arm64.tar.gz
```

Single-file native binaries, no runtime dependencies. macOS build is manual-trigger only
(expensive runners).

---

## Key conventions for agents

- **Branch from develop**, never from master. Feature branches: `features/<name>`.
- **Cargo.toml version** on develop may be one version ahead of the deployed binary — that
  is expected due to `copy-to-common.ps1` deploy hook. Never flag as inconsistency.
- **`cargo check` / `cargo clippy`** for iteration. Never `cargo build --release`.
- **Never write separate `AGENTS_xxx.md` sibling files** unless explicitly requested.
  OpenCode reads `AGENTS.md` only. Out-of-repo planning goes to
  `C:\WorkArea\AI\codesearch\instructions\`.
- **Path normalization**: all path comparisons must go through a single normalize utility.
  Windows UNC prefixes (`\\?\C:\`), backslash/forward-slash mismatches, and worktree
  `.git` file resolution have each caused subtle bugs in the past.
- **ONNX arena allocator**: uses `kNextPowerOfTwo` growth, never returns memory to OS.
  ~2GB memory during indexing is a known limitation. No local fix available until upstream
  fastembed exposes `OrtArenaCfg`.
- **Git worktrees**: `find_git_root` returns the worktree directory itself (fixed).
  Each worktree is a separate indexable repo. Groups can be used to search across worktrees
  of the same base repo.

---

## Active feature branches (not yet merged)

| Branch | Description |
|---|---|
| `features/symbol-references` | `find_impact` MCP tool, C# SCIP helper, blast-radius analysis |

---

## Changelog highlights (recent)

- **v1.0.90** — `codesearch serve tui` standalone TUI, `--no-tui` flag, `GET /status` endpoint
- **v1.0.86** — Strict `get_chunk` scoping in multi-repo mode; zombie-proof idle reaper
- **v1.0.85** — `codesearch doctor` with `--all` and `--repo` flags
- **v1.0.84** — rmcp 0.9.1 → 1.5.0 (Claude Code 2.1.x protocol fix)
