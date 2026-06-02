# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).



## [1.0.162] - 2026-06-02

### Fixed

- **Windows: flaky relocation tests eliminated** — `std::fs::rename` and
  `std::fs::remove_dir_all` on Windows fail with "Access is denied" when a
  git subprocess from `init_git_remote` keeps a directory handle open after
  exit. All 7 rename calls in `repos.rs` tests now use a `rename_retry()`
  helper that retries up to 10 times with exponential back-off; the one
  `remove_dir_all` call is now best-effort (the test assertion holds either
  way). Verified stable across 3 consecutive full-suite runs (432 passed,
  0 failed).

## [1.0.160] - 2026-06-02

### Fixed

- **`evaluate_csharp_rebuild` no longer holds `config.write()` during git/fs I/O** —
  the bootstrap timestamp computation (git subprocess + ≤10 000-entry filesystem
  walk) previously ran while holding the `config` write-lock, blocking every
  concurrent `config.read()` caller for the full scan duration. The lock is now
  acquired only for the brief config update; the slow work runs with no lock held.
- **`evaluate_csharp_rebuild` offloaded to `spawn_blocking` in phase 2** — even
  after the lock fix, the function ran synchronously on a Tokio worker thread.
  Wrapped in `spawn_blocking` at the call site in `run_phase_2_csharp_scip` so
  the async runtime stays responsive while processing all C# candidates.
- **`build_index()` in warmup and add-repo background task now use `spawn_blocking`** —
  two sites called the CPU-heavy HNSW `build_index()` directly on async threads.
  Both now follow the established pattern (`spawn_blocking` + `blocking_write()`).
- **`reload_if_changed` uses `safe_canonicalize`** — replaces the raw
  `std::fs::canonicalize` that could leave Windows `\\?\` UNC prefixes on the
  config path, causing path comparisons to silently fail.
- **Accurate doc-comments on `relocate_missing` / `prune_stale`** — both
  methods perform disk I/O (filesystem traversal, git subprocess) and should
  be called via `spawn_blocking` in async contexts; the comments now say so.
- **`ensure_hnsw_index_if_needed` extracted and tested** — the safety-net HNSW
  rebuild logic (detects and repairs a DB with chunks but no index, e.g. after
  cancellation) is now a named `pub(crate)` function with 3 unit tests
  (unindexed-with-chunks rebuilds, already-indexed is idempotent, empty DB skips).
- **`metadata.json` schema consistency** — the normal index path now writes
  `"partial": false` so readers always find the field regardless of whether
  indexing completed or was cancelled.
- **Cancellation finalisation is best-effort** — metadata write, FileMetaStore
  save, and stats read in the cancel path now log-and-continue on failure
  instead of propagating `Err`, so the partial chunks remain searchable even
  if any recovery step fails.

## [1.0.156] - 2026-06-02

### Fixed

- **`reconcile_all_paths` no longer blocks the Tokio async runtime** — the
  function spawns git subprocesses and holds the config `RwLock` write-guard
  while scanning the filesystem. It is now offloaded via
  `tokio::task::spawn_blocking` so Tokio worker threads stay responsive during
  startup reconciliation.
- **Phase 1 auto-prune now honours `config_path_override`** — the prune path
  wrote `repos.json` via `config.save()`, bypassing `ServeState::persist_config`.
  All save sites in `ServeState` must route through `persist_config` so the
  override (used in integration tests) is respected. Fixed to use
  `self.persist_config(&config)`.



## [1.0.154] - 2026-06-02

### Fixed

- **Windows CI: path-comparison failures in relocation tests** — `scan_for_remote`
  now canonicalizes discovered paths via `safe_canonicalize()` before recording
  them, resolving 8.3 short names (e.g. `RUNNER~1`) to their long-name form
  (`runneradmin`). Test assertions updated to use the same canonicalization so
  `tempfile::tempdir()` short-name paths and `read_dir` long-name paths compare
  equal on Windows.

## [1.0.153] - 2026-06-02

### Added

- **Auto-prune stale repos during Phase 1 warmup** — when a repo fails warmup
  because its path or database no longer exists, `codesearch serve` now
  automatically removes it from `repos.json` and logs a warning, instead of
  silently retrying on every restart. Works in concert with the relocation pass
  (reconcile_all_paths): relocatable repos are rewritten first, truly missing
  ones are pruned.

### Fixed

- **Missing `YELLOW` color variable in `scripts/qc.sh`** — the variable was
  referenced but never declared, causing a visual glitch in QC output.

## [1.0.152] - 2026-06-02

### Added

- **Best-effort relocation of moved/renamed repositories** — every repo's git
  remote (`remote.origin.url`) is now captured at registration. When a
  registered folder is renamed or moved, `codesearch serve` no longer crashes:
  on startup it reconciles all paths, and for each missing path it scans nearby
  folders (bounded depth, override with `CODESEARCH_RELOCATE_MAX_DEPTH`, default
  `3`) for a git checkout with the same remote. A single unambiguous match is
  rewritten into `repos.json`; ambiguous/absent matches are logged and skipped
  (the dead path is never indexed). Phase-2 (C# SCIP) and Phase-3 (pre-warm)
  also guard `path.exists()` so a stale path can never reach heavy code paths.
- **`codesearch index prune`** — new command that relocates moved repos first,
  then unregisters any remaining stale entries, printing a summary.

### Changed

- **The user-settable `--alias`/`-a` flag was removed from `index add`** — the
  alias (the `repos.json` key, used by groups and the MCP `project` argument) is
  now always derived from the repository directory name. In practice the alias
  always had to equal the directory name, so a custom alias only caused
  downstream mismatches. The `index symbol <alias>` positional (a lookup key) is
  unchanged.

### Fixed

- **A hand-edited or corrupt-ish `repos.json` no longer crashes the app** — on
  load the config is reconciled in memory: entries with empty/blank alias keys
  are dropped, orphaned `repos_meta` is removed, and group members referencing
  unknown aliases (and groups left empty) are pruned. Valid aliases are never
  renamed (that would break group references).

## [1.0.146] - 2026-06-02

### Added

- **Semantic Markdown chunking** — Markdown files (`.md`, `.markdown`, `.txt`) are
  now parsed with the **tree-sitter-md block grammar**, so chunks align to sections,
  headings, and code fences instead of arbitrary line ranges. `Language::Markdown`
  now reports `supports_tree_sitter() == true` and has a compiled-in grammar.

### Changed

- **Supported-languages documentation corrected** — the README language table now
  lists all 15 tree-sitter languages actually supported (Rust, Python, JavaScript,
  TypeScript, C, C++, C#, Go, Java, Shell, Ruby, PHP, YAML, JSON, Markdown);
  it previously showed only 9, omitting Shell, Ruby, PHP, YAML, JSON, and Markdown.

## [1.0.142] - 2026-06-01

### Fixed

- **`codesearch serve` became unresponsive during startup warmup** — heavy
  synchronous work (`FileWalker::walk`, `VectorStore::build_index` HNSW
  construction, and fastembed/ONNX embedding which saturates all CPU cores)
  ran directly on tokio worker threads while warming up repos at startup. This
  starved the async runtime so `/health` timed out (>3s), causing
  `codesearch index` to report "serve did not respond in time". That work is
  now offloaded to `tokio::task::spawn_blocking`, keeping the async executor
  responsive: serve answers `/health` and accepts `POST /repos[/:alias/reindex]`
  immediately during warmup, returning 202 and running the index job in the
  background (accept-and-defer) instead of making the client wait or fail.
  Lock safety: every async `RwLock` guard is released before the blocking task
  acquires `blocking_write()` on the same store, so there is no lock-over-await
  deadlock.


## [1.0.141] - 2026-06-01

### Fixed

- **`codesearch index` aborted instead of waiting when serve was warming up** —
  on `ServeUnresponsive` the CLI returned an error. It now waits patiently
  (`serve_delegate_with_warmup_wait`): prints progress and retries every 8s up
  to ~2 min, delegating as soon as serve becomes ready, and only erroring if the
  budget is exhausted. (Superseded for the responsiveness root cause by 1.0.142.)
- **409 Conflict when recreating a missing database** — when a registered repo's
  database was gone, the CLI's auto-register returned 409 ("already registered")
  and fell back to a local duplicate. It now retries as
  `POST /repos/{alias}/reindex?force=true`, which recreates the DB via serve.


## [1.0.140] - 2026-06-01

### Fixed

- **Last raw `.canonicalize()` eliminated** — `get_db_path_smart` still used the
  old `normalize_path(&p.canonicalize()...)` pattern. Routed through the central
  `safe_canonicalize()` so no raw `.canonicalize()` remains outside its own
  definition.


## [1.0.139] - 2026-06-01

### Changed

- **Central path canonicalization** — introduced `safe_canonicalize()` and
  `strip_unc_prefix()` in `crate::cache` as the single approved way to
  canonicalize paths, and replaced all 16+ raw `.canonicalize()` call sites
  across `repos.rs`, `db_discovery/mod.rs`, `index/mod.rs`, `lmdb_registry.rs`,
  and `serve/mod.rs`. This structurally prevents the recurring Windows UNC-path
  (`\\?\`) bug class. Policy documented in `AGENTS.md`; 6 regression tests added.


## [1.0.138] - 2026-06-01

### Fixed

- **`\\?\`-prefixed UNC paths stored in repos.json caused spurious "Database
  not found" errors** — `Path::canonicalize()` on Windows returns an
  extended-length UNC path (`\\?\C:\...`). When stored verbatim in
  `repos.json`, downstream `.join(".codesearch.db")` and `Path::exists()`
  calls failed inconsistently (e.g. `\\?\C:\foo\.codesearch.db` returned
  `false` even when `C:\foo\.codesearch.db` existed). This affected 7 repos
  in repos.json and caused a cascade of "Database not found" 500 errors and
  fallbacks to local duplicate indexes. `register()` and `register_with_alias()`
  now strip the `\\?\` prefix before storage so repos.json always holds plain
  `C:\...` paths. Existing UNC entries are automatically corrected at the next
  registration. (Existing repos.json was also patched in-place.)
- **500 "Database not found" on reindex caused a local duplicate index** —
  when a registered repo's database was deleted externally (e.g. serve killed
  mid-index), the reindex endpoint returned 500 "Database not found". The CLI
  treated this as a generic failure and fell back to local indexing, recreating
  the duplicate. It now triggers the same auto-register (`POST /repos`) path as
  a 404, which recreates the database via serve without any local fallback.



## [1.0.137] - 2026-06-01

### Fixed

- **`codesearch index` silently created a local duplicate index when `serve`
  was busy starting up** — the CLI probes `serve`'s `/health` before delegating.
  Any failure (including a *timeout* while `serve` was warming up its repos) was
  treated as "serve is not running", so the CLI silently fell back to creating a
  **local index** — a duplicate that `serve` does not manage and that can cause
  LMDB file-lock conflicts. The health probe now distinguishes three cases:
  *responsive* (delegate), *connection refused / not running* (index locally —
  detected immediately, so the local path is not slowed down), and *listening
  but unresponsive* (serve is up but busy). In the last case the CLI now
  **refuses to create a local duplicate** and asks you to retry shortly or stop
  `serve` first, instead of silently duplicating. The fallback is never silent
  anymore.
- **`codesearch index` could not register a brand-new repo via a running
  `serve` instance** — when `serve` was running and you indexed a repo that
  was not yet known to it, the auto-register call (`POST /repos`) failed with
  a misleading *"Database is locked by another process"* error and HTTP 500.
  Root cause: `SharedStores::new()` tried to acquire the writer lock
  (`.writer.lock`) *before* the `.codesearch.db` directory existed, so opening
  the lock file failed with "path not found" and was reported as a lock
  conflict. Consequences: the `repos.json` registration was rolled back (the
  alias was never persisted) and the CLI silently fell back to creating a
  **local duplicate index** instead of handing control to `serve`. Existing
  repos (whose database directory already existed) and local-only indexing
  were unaffected. The database directory is now created before the writer
  lock is acquired.
- **Genuine filesystem errors during database creation were masked as lock
  contention** — a real I/O failure (e.g. permission denied) while creating
  the database directory now surfaces as itself instead of the misleading
  "Database is locked by another process" message.

### Changed

- **Serve config writes now honor the configured config path** — all
  `repos.json` writes from the register/remove/metadata-persist paths route
  through `ServeState::persist_config()`, which respects the active config
  path override. Production behavior is unchanged; this makes the
  register/remove path hermetically testable.

### Tests

- Added regression guards that exercise the brand-new-repo store-creation and
  register path with the `.codesearch.db` directory genuinely absent
  (`try_open_stores`, `SharedStores::new`, `acquire_writer_lock`, and an
  end-to-end `add_repo_handler` test asserting 202 + no `repos.json`
  rollback). These were verified to fail against the pre-fix code.
- Added guards for the serve `/health` probe classification: a responsive
  endpoint → delegate, and a listening-but-slow endpoint → "unresponsive"
  (caller refuses to create a local duplicate).


## [1.0.135] - 2026-05-27

### Fixed

- **MCP local/stdio mode ignores `project`/`group` params** — when running
  `codesearch mcp` without `codesearch serve` (Local mode), passing `project` or
  `group` parameters caused a hard error: *"project/group routing requires
  `codesearch serve` to be running."* The LLM (Claude Code) auto-fills these
  params from the tool schema. Now they are silently ignored with a warning log,
  and the local database is used. Closes #65.
- **QC script `YELLOW` color variable undefined** — `scripts/qc.sh` referenced
  `YELLOW` without defining it, causing `set -u` failures on Linux. Fixed by
  adding the missing color constant.

### Changed

- **`protect-master.yml` allows `release/*` branches** — CI branch protection
  workflow now accepts PRs from both `develop` and `release/*` branches into
  `master`, enabling clean release branches when develop has diverged.


## [1.0.132] - 2026-05-22

### Added

- **Tree-sitter grammars for Bash, Ruby, PHP, YAML, JSON** — codesearch now
  supports AST-aware chunking for 14 languages total (previously 9: Rust,
  Python, JavaScript, TypeScript, C, C++, C#, Go, Java). Closes #55.
- **Bash equivalents of QC and bump-version scripts** — `scripts/qc.sh` and
  `scripts/bump-version.sh` for Linux/macOS environments, complementing the
  existing PowerShell scripts.
- **Platform-aware pre-push hook** — `.git/hooks/pre-push` auto-detects the
  platform and calls the appropriate QC script before allowing a push.
- **CodeQL configuration** — added `.github/codeql/codeql-config.yml` to
  suppress `rust/path-injection` false positives (codesearch is a local dev
  tool, not a web-facing server).

### Changed

- **SCIP LMDB map_size raised from 64 MB to 512 MB** — the SCIP symbol index
  LMDB environment now defaults to 512 MB virtual address space, up from 64 MB.
  This prevents `MDB_MAP_FULL` errors on large solutions. Override with
  `CODESEARCH_SCIP_LMDB_MAP_MB` environment variable.
- **Centralized DB open/create logic** — extracted `try_open_stores()` to
  eliminate duplicate LMDB open paths across the codebase. All serve-context
  LMDB access now goes through a single entry point.

### Fixed

- **LMDB double-open race in `add_repo_handler`** — a concurrent guard with
  cancel token now prevents two simultaneous `add_repo` calls from opening the
  same LMDB database, which caused panics and corrupted indexes.
- **LMDB double-open in MCP fallback path** — blocked a code path where the
  MCP handler could open a second LMDB environment on the same directory when
  `SharedStores` initialization failed.
- **`TrackedEnv` runtime guard** — a new runtime guard detects LMDB
  double-open attempts at runtime, producing a clear error instead of a panic.
- **Force-reindex on missing database** — `try_open_stores()` now creates the
  database on the fly when it's missing, fixing the case where a previously
  registered repo had no `.codesearch.db` directory yet.
- **Explore two-pass fallback** — `explore outline` now falls back to a
  second lookup strategy when the alias name matches a package subdirectory,
  preventing empty results on certain project layouts.
- **TUI C# indexing status** — Phase 2 SCIP rebuilds and Phase 3 pre-warm now
  correctly signal the TUI `indexing_cb`, so the UI shows "C# Indexing"
  during background symbol operations.
- **FSW SCIP rebuild TUI signal** — file-watcher-triggered symbol rebuilds now
  update `active_reindexes` so the TUI displays the correct indexing state.
- **CI test resilience** — `test_indexer_returns_empty_when_db_missing` is now
  resilient to LMDB lock contention on CI runners.
- **Protect-master workflow** — GitHub Actions workflow that only allows PRs
  from `develop` to `master`, preventing accidental direct pushes.
- **`config.save()` failure warnings** — `add_repo_handler` now logs warnings
  when `config.save()` fails instead of silently dropping the error.



## [1.0.97] - 2026-05-15

### Fixed

- **CLI auto-register retry race** — after auto-registering a new repo (POST
  `/repos` → 202 Accepted), the CLI no longer retries the reindex immediately.
  The previous retry raced with the background indexing task and always failed
  with "Database not found" because the LMDB database hadn't been created yet.
- **`cargo fmt` CI failures** — pinned toolchain in `rust-toolchain.toml` and
  updated local rustfmt (1.92 → 1.95) to match CI.



## [1.0.96] - 2026-05-14

### Fixed

- **`add_repo_handler` deadlock** — POST `/repos` was calling `index_quiet()`
  inline, causing a deadlock when the serve's own startup (Phase 1) still held
  the LMDB lock. Indexing now runs in a `tokio::spawn` background task and the
  handler returns `202 Accepted` immediately, matching the `reindex_handler`
  pattern. This fixes the "fresh install → serve hangs" scenario on both Linux
  and Windows.



## [1.0.95] - 2026-05-14

### Added

- **POST /reload endpoint** — forces `repos.json` reload from disk, even if
  the file mtime hasn't changed. Used by the TUI `[s]` key to pick up
  externally added/removed repos without restarting serve.
- **TUI `[s]` key** — both embedded and remote TUIs now support `[s]` to
  manually reload `repos.json`, picking up repos added via `codesearch index add`
  or other external changes.
- **CLI auto-register on 404** — `codesearch index -f` from a directory not
  yet in `repos.json` now auto-registers the repo with the running serve
  instance (via `POST /repos`) instead of falling back to local indexing,
  which caused LMDB file-lock conflicts.

### Changed

- **Removed vendor name references** from docs and comments for a cleaner
  public repository.



## [1.0.94] - 2026-05-08

### Added

- **C# semantic analysis helper (`scip-csharp`)** — a small .NET 10 CLI tool
  that wraps Roslyn's `SymbolFinder.FindReferencesAsync()` and produces a
  symbol reference index. Framework-dependent, ~5–15 MB. Bundled in the new
  `-with-csharp` release variants, or available via `$PATH` / env var override.
- **`-with-csharp` release variants** — pre-built release archives that include
  the `scip-csharp` helper alongside the codesearch binary. There are now 6
  release archives total: 3 plain `codesearch` packages and 3
  `codesearch-with-csharp` packages (Windows, Linux, macOS). The plain
  packages are unchanged for users who don't need C# symbol references.
- **Dedicated C# README** — all C#-specific goal, operation, installation, and
  testing instructions now live in `README_CSharp.md`; the main README only
  links there so non-C# users can skip the extra detail.
- **`.cs` file watcher debounce** — 60-second quiet period after `.cs` file
  changes triggers an automatic symbol index rebuild. Buffer is cleared on git
  branch switches to avoid stale rebuilds.
- **`symbols=true` query parameter** on the serve reindex endpoint
  (`POST /repos/:alias/reindex?force=true&symbols=true`) for forced symbol
  index rebuilds.

### Changed

- **Architecture is language-agnostic**: the `SymbolIndexer` trait and
  per-language adapter pattern are in place. Future branches can add Python
  (scip-python), TypeScript (scip-typescript), Rust (scip-rust), etc. without
  redesigning.

### Breaking

- **LMDB format change** — existing `scip` LMDB databases require a
  full rebuild after this upgrade. The first `find_impact` call (or
  explicit `reindex?symbols=true`) will trigger an automatic rebuild.
  No manual data migration is needed.

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
  starts the idle timer at warmup so background-warmed aliases (ExampleRepo,
  DPS, ExampleRepo and others showing `Last Tool Call = -` indefinitely) are
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
  projects (e.g. all related microservice repos).

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

[1.0.132]: https://github.com/flupkede/codesearch/compare/v1.0.97...v1.0.132
[1.0.97]: https://github.com/flupkede/codesearch/compare/v1.0.96...v1.0.97
[1.0.96]: https://github.com/flupkede/codesearch/compare/v1.0.95...v1.0.96
[1.0.95]: https://github.com/flupkede/codesearch/compare/v1.0.94...v1.0.95
[1.0.94]: https://github.com/flupkede/codesearch/compare/v1.0.93...v1.0.94
[1.0.93]: https://github.com/flupkede/codesearch/compare/v1.0.81...v1.0.93
[1.0.81]: https://github.com/flupkede/codesearch/compare/v1.0.77...v1.0.81
[1.0.77]: https://github.com/flupkede/codesearch/compare/v1.0.74...v1.0.77
[1.0.74]: https://github.com/flupkede/codesearch/compare/v1.0.72...v1.0.74
[1.0.72]: https://github.com/flupkede/codesearch/releases/tag/v1.0.72
