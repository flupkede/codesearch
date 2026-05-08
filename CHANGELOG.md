# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).



## [1.0.94] - 2026-05-08

### Fixed

- **`status(projects)` now returns real chunk counts** for unopened repos by
  persisting them in `metadata.json` after every indexing operation (B2).
- **Double chunks on reindex** — guard clears both stores when `FileMetaStore`
  is empty but `VectorStore` has data, preventing full duplication (B1).
- **Regex `\w+`/`\b`/`\d` broken in literal mode** — extracts clean BM25 tokens
  from regex patterns for candidate generation while preserving full regex for
  post-filter (B3).
- **Duplicate definitions in `find_impact`** — `FindCommonRoot` now uses all
  solution projects instead of filtered subset for consistent relative paths (B4).
- **`codesearch index` now always delegates to running serve** (not just `-f`),
  preventing LMDB file-lock conflicts between CLI and serve.
- **`release.ps1` path resolution** — fixed .NET `ReadAllText` resolving against
  wrong CWD by using absolute paths derived from script location.

## [1.0.93] - 2026-05-08

### Changed

- **Local QC gate** (`scripts/qc.ps1`) — mirrors CI checks locally
  (`fmt → check → clippy → test --lib → test --test *`) and
  includes pre-push hook (`scripts/pre-push`) that blocks pushes when QC fails.
  Prevents recurring "local pass, CI fail" problems.
- **CodeQL configuration** — added `.github/codeql/codeql-config.yml` to suppress
  `rust/path-injection` false positives (codesearch is a local dev tool,
  not a web-facing server). In-repo CodeQL workflow configured to use this config.

### Fixed

- **`test_gitignore_rules_respected`** — gitignore directory patterns like `obj/`,
  `bin/`, `.claude/` now correctly match nested files. The `is_gitignored()`
  method iterates over all path components with `is_dir=true` so that
  directory-only patterns match files inside them.
- **Clippy `unnecessary_sort_by`** — replaced `sort_by()` with `sort_by_key()`
  in two locations in `src/serve/mod.rs` to avoid lint failure on CI.

## [Unreleased]

- **Dedicated C# README** — all C#-specific goal, operation, installation, and
  testing instructions now live in `README_CSharp.md`; the main README only
  links there so non-C# users can skip the extra detail.
- **C# semantic analysis helper (`scip-csharp`)** — a small .NET 10 CLI tool
  that wraps Roslyn's `SymbolFinder.FindReferencesAsync()` and produces a
  symbol reference index. Framework-dependent, ~5–15 MB. Bundled in the new
  `-with-csharp` release variants, or available via `$PATH` / env var override.
- **`-with-csharp` release variants** — pre-built release archives that include
  the `scip-csharp` helper alongside the codesearch binary. There are now 6
  release archives total: 3 plain `codesearch` packages and 3
  `codesearch-with-csharp` packages (Windows, Linux, macOS). The plain
  packages are unchanged for users who don't need C# symbol references.
- **`.cs` file watcher debounce** — 60-second quiet period after `.cs` file
  changes triggers an automatic symbol index rebuild. Buffer is cleared on git
  branch switches to avoid stale rebuilds.
- **`symbols=true` query parameter** on the serve reindex endpoint
  (`POST /repos/:alias/reindex?force=true&symbols=true`) for forced symbol
  index rebuilds.

### Fixed

- **JSON version validation** — `parse_json_index()` now rejects scip-csharp
  index versions other than `"1.0"`, preventing silent breakage when the
  helper is updated to an incompatible format.
- **Helper detection failure cache** — `detect_helper()` now caches
  "helper not found" results, eliminating repeated PATH lookups and
  subprocess probes on every MCP `find_impact` request when the helper
  is missing.
- **Bincode schema versioning** — all LMDB payloads now include a
  version byte. Reading data from a future schema version produces a
  clear error with rebuild instructions instead of silent corruption.
- **Shared `SymbolIndexerRegistry`** — `find_impact` (MCP) and
  `trigger_symbol_rebuild` (HTTP) now reuse the shared registry from
  `ServeState` instead of creating fresh instances per request,
  restoring cache effectiveness.
- **O(1) position lookup** — `find_references_by_position` now uses a
  secondary `scip_positions` LMDB table instead of iterating and
  deserializing every symbol in the database.
- **O(1) fuzzy lookup** — `find_references` fuzzy fallback now uses a
  secondary `scip_simple_names` LMDB table instead of a full-table
  scan with bincode deserialization.
- **Removed `#[allow(dead_code)]`** on 5 SCIP constants in
  `constants.rs` that are now actively referenced from `csharp.rs`.

### Breaking

- **LMDB format change** — existing `scip` LMDB databases require a
  full rebuild after this upgrade. The first `find_impact` call (or
  explicit `reindex?symbols=true`) will trigger an automatic rebuild.
  No manual data migration is needed.

### Changed

- **Architecture is language-agnostic**: the `SymbolIndexer` trait and
  per-language adapter pattern are in place. Future branches can add Python
  (scip-python), TypeScript (scip-typescript), Rust (scip-rust), etc. without
  redesigning.

## [1.0.81] - 2026-05-02

### Added

- **`codesearch serve tui`** — standalone sub-action that opens the ratatui TUI
  connected to a running serve instance via HTTP polling. The TUI can be opened
  and closed independently of the server.
- **`codesearch serve --no-tui`** — start serve headless even when a TTY is
  available. Typical workflow: `codesearch serve --no-tui` in one terminal,
  `codesearch serve tui` in another.
- **`GET /status` endpoint** on serve returns a JSON snapshot of all repo
  states, sessions, and CPU usage — usable for external monitoring and the
  standalone TUI.

### Fixed

- **Idle eviction now covers warmed-but-never-queried repos**: `warmup_repo`
  starts the idle timer at warmup so background-warmed aliases (KRKA.Aprimo,
  DPS, BAYR.Aprimo and others showing `Last Tool Call = -` indefinitely) are
  evicted by the idle reaper instead of holding LMDB envs and embedder state
  forever.
- **Ctrl-C no longer quits the TUI**: crossterm's raw mode delivers Ctrl-C as
  a key event, bypassing the OS-level handler. Treating it as quit was a
  foot-gun: a stray Ctrl-C in the wrong terminal would tear down the whole
  serve. Use `q` only.

### Changed

- **`unsafe` blocks documented**: SAFETY comments added to the three LMDB
  env-open `unsafe` blocks in `src/embed/cache.rs` and `src/vectordb/store.rs`.

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

[1.0.94]: https://github.com/flupkede/codesearch/compare/v1.0.93...v1.0.94
[1.0.93]: https://github.com/flupkede/codesearch/compare/v1.0.77...v1.0.93
[1.0.77]: https://github.com/flupkede/codesearch/compare/v1.0.75...v1.0.77
[1.0.75]: https://github.com/flupkede/codesearch/compare/v1.0.74...v1.0.75
[1.0.74]: https://github.com/flupkede/codesearch/compare/v1.0.72...v1.0.74
[1.0.72]: https://github.com/flupkede/codesearch/releases/tag/v1.0.72
