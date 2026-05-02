# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.82] - 2026-05-02

### Fixed

- `codesearch index list` now actually lists all repositories registered in
  `~/.codesearch/repos.json` instead of only checking the current directory.
  A loose `.codesearch.db` in an unregistered directory is shown separately
  under "Local (unregistered)". Both TODO markers removed.

## [1.0.77] - 2026-05-01

### Removed

- Stale planning documents (`.docs/`) and old benchmark results (`benchmarks/`)
  removed from the repository. These were internal working documents with no
  value for contributors.

## [1.0.74] - 2026-05-01

### Fixed

- **MCP session keep_alive timeout removed**: the previous 30-minute idle timeout
  was killing sessions mid-working-day. Sessions now live until TCP dies, which
  is the correct behaviour for a local single-user long-running serve process.

## [1.0.72] - 2026-05-01

First stable release of codesearch — a Rust-based hybrid (vector + BM25 + AST)
code search MCP server, optimised for AI coding agents working across many
repositories.

### Added

- **Multi-repository serve mode** (`codesearch serve`): a long-running HTTP/SSE
  process that holds many indexed repositories warm at the same time, with
  per-project routing via `project=…`, group routing via `group=…`, and
  cross-repository search using RRF fusion across project boundaries.
- **Stdio proxy with auto-reconnect**: `codesearch mcp` (stdio mode) detects a
  running `serve` process and proxies tool calls to it. The proxy now performs
  client-side retries with a forced reconnect when it sees a transport-level
  failure (broken TCP keep-alive, stale session 404, server restart, laptop
  suspend) so MCP clients like Claude Desktop self-heal transparently. After a
  serve restart the first call returns a clear "reconnecting" message and the
  next call succeeds.
- **MCP tool surface optimised for agents** to reduce grep-fallback behaviour:
  - `search` (semantic / hybrid / lexical / pure-literal regex modes)
  - `find` (definition / usages / imports / dependents)
  - `explore` (file outline / similar chunks)
  - `get_chunk` for cheap follow-up reads of a specific code chunk
  - `status` (index / projects)
- **Tree-sitter AST-aware chunking** for 9 languages: Rust, Python, JavaScript,
  TypeScript, C, C++, C#, Go, Java.
- **Persistent embedding cache** keyed on SHA-256 of chunk content, surviving
  `--force` rebuilds and per-file re-indexes.
- **Git worktree support**: when `.git` is a worktree marker file (not a
  directory), the project root is correctly resolved to the worktree itself.
- **Long UNC-path support** on Windows for repositories under `\\?\C:\…` paths.
- **Repository groups** for cross-repo search across user-defined sets of
  projects (e.g. all *.Aprimo* repos).

### Changed

- **Search quality**: re-tuned RRF fusion of the vector / BM25 / exact-identifier
  signals so common tool names and exact strings are no longer drowned out by
  semantic neighbours, reducing the rate at which agents fall back to external
  grep.
- **Idle eviction**: only refreshes a project's "last accessed" timestamp on a
  direct query against that project, not on fan-out queries that touch the
  index merely because they routed through the same group.
- **TUI CPU%**: now normalised by core count.

### Fixed

- **Security**: validate `CODESEARCH_CONFIG` environment variable against a path
  traversal pattern (CodeQL finding). Config path is now rejected if it contains
  `..` segments, preventing a directory traversal via env var.
- **Issue #30** ([LMDB resize crash on large repositories](https://github.com/flupkede/codesearch/issues/30)):
  When the database grew beyond its initial allocation (`MDB_MAP_FULL`), the
  resize failed with `"an environment is already opened with different options"`.
  The fix closes and reopens the LMDB environment around the resize, allowing
  codesearch to index large repositories (tested: 4400+ files, 89 MB) without
  crashing.
- File-change tracking and reaper visibility in `serve` mode.

### Removed

- Server-side transparent MCP session-reconnect middleware: replaced by the
  client-side retry in the stdio proxy. The middleware could not reach
  non-compliant remote MCP clients (their HTTP pool gives up at the TCP layer
  before the request hits the server) and added a session-counter leak.

### Known limitations

- Remote MCP clients that do not handle 404 "Session not found" per the MCP
  spec (e.g. OpenCode 1.14.x at the time of writing) need to be restarted after
  a `codesearch serve` restart.
- `codesearch serve` keeps one writer per database (LMDB invariant). Concurrent
  reindex from a second process is rejected.

[1.0.77]: https://github.com/flupkede/codesearch/compare/v1.0.75...v1.0.77
[1.0.75]: https://github.com/flupkede/codesearch/compare/v1.0.74...v1.0.75
[1.0.74]: https://github.com/flupkede/codesearch/compare/v1.0.72...v1.0.74
[1.0.72]: https://github.com/flupkede/codesearch/releases/tag/v1.0.72
