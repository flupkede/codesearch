# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).


## [1.1.0] - 2026-07-01

**Federation release.** This version lands **federation** — the ability to fan read queries out to remote `codesearch serve` peers and manage their indexes from the local CLI — plus a README security analysis of the feature and several fixes.

### Added

- **Federation — remote peers.** Register peers with `codesearch remote add/rm/list` (local `~/.codesearch/repos.json` config), reference them from groups via `@peer` (e.g. `"docs": ["@cloud"]`), and `codesearch` fans `search`/`get_chunk` out over TLS, merging remote and local results with Reciprocal Rank Fusion (RRF). Remote misses degrade to local-only results with a `warnings` field — they never hard-fail.
- **Remote index management (`--remote`).** The `index` verbs now take `--remote <peer>` to operate against a peer: `index list/add/rm/reindex --remote cloud` drive the peer's management REST API (`GET /status`, `POST /repos`, `DELETE /repos/:alias`, `POST /repos/:alias/reindex`). New `index reindex` verb (local + remote). `--json` on `list`/`reindex` (requires `--remote`).
- **Cloud deployment topology** — split indexer job (4 vCPU/8 GiB, builds + uploads a snapshot) and read-only restore-only serve replica (1 vCPU/2 GiB) for scale-to-zero hosting. See `docs/federation-cloud-deployment.md`.
- **README `## Security` section** documenting the federation trust model, secret storage/transport, redirect handling, serve-side enforcement, and cross-instance isolation.

### Fixed

- **`active_sessions` overflowed to `u64::MAX`** on every REST (`/search`, `/find`, `/explore`, `/get_chunk`) request: per-request `CodesearchService`s were decrementing the session counter on `Drop` without ever incrementing it. Gated the decrement behind a `tracks_session` flag so only genuine MCP sessions balance the counter.
- **`index rm <arg>`** now resolves the argument as a **registered alias first**, falling back to path interpretation only when it isn't one (previously a bare alias failed with an OS path error).
- Added an `ls` visible alias to the `index`/`groups`/`remote` `list` subcommands.

### Changed

- Pre-GA changelog history (`[1.0.72]`–`[1.0.208]`) condensed to one-line summaries to mark the GA cutover; no entry was dropped and the key facts survive in the summaries. Full detail for the latest pre-GA release (`[1.0.209]`) is preserved verbatim below.

## [1.0.209] - 2026-06-17

### Fixed

- **Repo stuck showing "Indexing" in the TUI forever**: `ServeState.active_reindexes`
  was an in-memory `DashSet<String>` with no expiry. Background indexing tasks run
  inside fire-and-forget `tokio::spawn` calls whose `JoinHandle` is discarded, so a
  panic or cancellation between insert and remove silently leaked the entry —
  causing the TUI to show "Indexing" permanently and the `POST /repos/<alias>/reindex`
  endpoint to return `409 Conflict` forever, even though the actual index was
  complete. Converted to `Arc<DashMap<String, Instant>>` with self-healing
  semantics: entries older than `MAX_INDEXING_SECS` (30 min, overridable via
  `CODESEARCH_MAX_INDEXING_SECS`) are lazily evicted on read. Added
  `begin_indexing` / `end_indexing` / `is_indexing` helpers; the eviction path
  uses atomic `remove_if` to prevent a TOCTOU race that could wrongly drop a
  freshly-refreshed entry.

## [1.0.208] - 2026-06-14
- Fixed `doctor` LMDB double-open in the embedded TUI (live-stats registry fallback); documented develop-based gitflow in `AGENTS.md`/`AGENTS.develop.md`.

## [1.0.207] - 2026-06-12
- Added `serve --host`, global `.codesearchignore`, Jupyter/Dart language support, TUI `r` (remove) key, and git worktree auto-index hook; fixed LMDB reopen "already opened with different options" 500 and FSW repo-local `.codesearchignore`/`.git/info/exclude` loading.

## [1.0.171] - 2026-06-04
- Security hardening: API key auth on management endpoints, path-containment allowlist (`CODESEARCH_ALLOWED_ROOTS`), C# path-traversal and command-injection fixes, and GitHub Actions pinned to SHAs with least-privilege permissions.

## [1.0.162] - 2026-06-02
- Eliminated flaky Windows relocation tests via a `rename_retry()` exponential back-off helper (432 passed / 0 failed).

## [1.0.160] - 2026-06-02
- Offloaded `evaluate_csharp_rebuild`/`build_index` to `spawn_blocking`, stopped holding the config write-lock during git/fs I/O, routed `reload_if_changed` through `safe_canonicalize`, extracted+tested `ensure_hnsw_index_if_needed`, made cancellation finalisation best-effort.

## [1.0.156] - 2026-06-02
- Fixed `reconcile_all_paths` blocking the Tokio runtime (now `spawn_blocking`); Phase 1 auto-prune now honours `config_path_override` via `persist_config`.

## [1.0.154] - 2026-06-02
- Fixed Windows CI path-comparison failures by canonicalizing discovered paths via `safe_canonicalize()` (8.3 short-name → long-name).

## [1.0.153] - 2026-06-02
- Added auto-prune of stale repos during Phase 1 warmup; fixed missing `YELLOW` var in `scripts/qc.sh`.

## [1.0.152] - 2026-06-02
- Added best-effort relocation of moved/renamed repos and `codesearch index prune`; REMOVED user-settable `--alias`/`-a` flag from `index add` (alias always derived from dir name); corrupt `repos.json` now reconciled instead of crashing.

## [1.0.146] - 2026-06-02
- Added semantic Markdown chunking via the tree-sitter-md block grammar; corrected README language table (15 tree-sitter languages).

## [1.0.142] - 2026-06-01
- Fixed serve unresponsive during startup warmup by offloading heavy sync work (FileWalker, HNSW `build_index`, ONNX embedding) to `spawn_blocking`; serve now answers `/health` and accept-and-defers `POST /repos` immediately.

## [1.0.141] - 2026-06-01
- CLI now waits patiently (≤~2 min) instead of aborting when serve is warming up; 409 on a missing DB now retried as `POST /repos/{alias}/reindex?force=true`.

## [1.0.140] - 2026-06-01
- Eliminated the last raw `.canonicalize()` by routing `get_db_path_smart` through the central `safe_canonicalize()`.

## [1.0.139] - 2026-06-01
- Added central `safe_canonicalize()`/`strip_unc_prefix()` in `crate::cache`, replaced 16+ raw `.canonicalize()` call sites, and documented the policy in `AGENTS.md` with 6 regression tests.

## [1.0.138] - 2026-06-01
- Fixed `\\?\` UNC paths stored in `repos.json` causing "Database not found" (prefix stripped at registration); fixed the 500 "Database not found" reindex local-duplicate fallback (now auto-registers via serve).

## [1.0.137] - 2026-06-01
- CLI no longer silently creates a local duplicate when serve is busy (health probe now distinguishes refused vs listening-but-unresponsive); fixed brand-new-repo "Database is locked" 500 (writer lock acquired after dir creation); serve config writes honour the configured path override; added regression guards.

## [1.0.135] - 2026-05-27
- Fixed MCP local/stdio mode erroring on `project`/`group` params (now ignored with warning, closes #65); fixed `YELLOW` var in `scripts/qc.sh`; `protect-master.yml` now allows `release/*` branches.

## [1.0.132] - 2026-05-22
- Added tree-sitter grammars for Bash/Ruby/PHP/YAML/JSON (14 langs total), bash QC/bump scripts + platform-aware pre-push hook, CodeQL config; raised SCIP LMDB map_size 64→512 MB; fixed LMDB double-open races (`TrackedEnv` runtime guard) and several explore/FSW/TUI status bugs.

## [1.0.97] - 2026-05-15
- Fixed CLI auto-register retry race (no longer re-reindexes before the LMDB DB exists); pinned toolchain for `cargo fmt` CI.

## [1.0.96] - 2026-05-14
- Fixed `add_repo_handler` deadlock by moving indexing to a `tokio::spawn` background task and returning `202 Accepted` immediately (fixes "fresh install → serve hangs").

## [1.0.95] - 2026-05-14
- Added `POST /reload` endpoint and TUI `[s]` key for manual `repos.json` reload; CLI auto-registers on 404 with a running serve (no local-duplicate fallback).

## [1.0.94] - 2026-05-08
- Added C# `scip-csharp` helper, `-with-csharp` release variants, and `.cs` watcher debounce (60s quiet period). BREAKING: LMDB format change — existing `scip` databases require a full rebuild (auto-triggered on first `find_impact`/`reindex?symbols=true`). Plus many `find_impact`, regex-literal, O(1) lookup, and reindex fixes.

## [1.0.93] - 2026-05-08
- Added local QC gate (`scripts/qc.ps1`) mirroring CI + pre-push hook, and CodeQL config; fixed gitignore directory-pattern matching (`obj/`, `bin/`, `.claude/`) and clippy lints.

## [1.0.81] - 2026-05-02
- Added `codesearch serve tui` standalone sub-action, `serve --no-tui`, and `GET /status`; fixed idle eviction for warmed-but-never-queried repos and Ctrl-C no longer quits the TUI.

## [1.0.77] - 2026-05-01
- Removed stale planning documents (`.docs/`) and old benchmark results (`benchmarks/`) from the repository.

## [1.0.74] - 2026-05-01
- Removed the 30-minute MCP session keep_alive timeout; sessions now live until TCP dies (correct for a local single-user long-running serve).

## [1.0.72] - 2026-05-01
- Initial multi-repo release: multi-repo `serve` (HTTP/SSE, per-project/group routing, RRF cross-repo search), stdio MCP proxy with client-side auto-reconnect, tree-sitter chunking (9 langs), persistent SHA-256 embedding cache, repository groups, re-tuned RRF, and LMDB resize crash fix (#30, `MDB_MAP_FULL`).

[1.0.171]: https://github.com/flupkede/codesearch/compare/v1.0.162...v1.0.171
[1.0.162]: https://github.com/flupkede/codesearch/compare/v1.0.160...v1.0.162
[1.0.160]: https://github.com/flupkede/codesearch/compare/v1.0.156...v1.0.160
[1.0.156]: https://github.com/flupkede/codesearch/compare/v1.0.154...v1.0.156
[1.0.154]: https://github.com/flupkede/codesearch/compare/v1.0.153...v1.0.154
[1.0.153]: https://github.com/flupkede/codesearch/compare/v1.0.152...v1.0.153
[1.0.152]: https://github.com/flupkede/codesearch/compare/v1.0.146...v1.0.152
[1.0.146]: https://github.com/flupkede/codesearch/compare/v1.0.142...v1.0.146
[1.0.142]: https://github.com/flupkede/codesearch/compare/v1.0.141...v1.0.142
[1.0.141]: https://github.com/flupkede/codesearch/compare/v1.0.140...v1.0.141
[1.0.140]: https://github.com/flupkede/codesearch/compare/v1.0.139...v1.0.140
[1.0.139]: https://github.com/flupkede/codesearch/compare/v1.0.138...v1.0.139
[1.0.138]: https://github.com/flupkede/codesearch/compare/v1.0.137...v1.0.138
[1.0.137]: https://github.com/flupkede/codesearch/compare/v1.0.135...v1.0.137
[1.0.135]: https://github.com/flupkede/codesearch/compare/v1.0.132...v1.0.135
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
