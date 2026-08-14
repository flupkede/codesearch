# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!--
Convention: there is no `[Unreleased]` staging section. New entries are added
directly under the heading for the current pending version (the one
`Cargo.toml` on `develop` is presently building toward — patch auto-bumps on
every PR merge, see RELEASING.md). That section keeps accumulating entries as
more PRs land; when the release is actually tagged, the same section is
finalized in place with a date — no renaming/migration step needed.
-->

## [1.2.11] (unreleased)

### Fixed
- **Literal-mode search results fabricated a `chunk_id: 0` that could silently resolve to the wrong file (todo #51).** `search(mode="literal")` hits carry no chunk id (literal search pinpoints a line, not a chunk), but both places that flatten literal results into the merged/federated response shape rendered the absent id as a real-looking `chunk_id: 0` (`.unwrap_or(0)`). A caller combining that fabricated 0 with the result source into a `get_chunk("<peer>/<alias>:0")` call got the wrong file back — no error, no ambiguity warning (reproduced against the federated `cloud/custom-kb` project). `SearchResultItem.chunk_id` is now `Option<u32>` and the field is omitted entirely for literal hits, so an id the server never returned cannot be constructed. Both fixed sites carry a red-verified regression pin. Also adds a store-level unit repro of the secondary cross-generation id-drift hypothesis (autoincrement ids are reused after a top-of-range delete on reopen), confirming the mechanism locally while the production two-cold-start comparison remains open — see AGENTS.md Open TODOs.

- **`codesearch index rm` could leave a repo unregistered while its (still-locked) database sat on disk untouched (todo #48).** The removal order was: unregister from `repos.json` and save it, *then* delete the `.codesearch.db` directory. When the delete failed — typically because a running `serve` instance (one the delegation probe didn't see: a second instance on another port, a stray CLI process, or `serve` having crashed without releasing its LMDB env) still held the files locked — the command errored out with the config entry already gone. The registry now claimed the repo didn't exist while its still-locked database remained on disk, with no way to clean it up except manually stopping the locking process; running the same `rm` command again did nothing, because `repos.json` no longer had an entry to remove. Fixed by reversing the order: the database directory is deleted first, and `repos.json` is only mutated once that succeeds. A failed delete now leaves the config untouched and prints an explicit "repos.json was NOT modified" message, so re-running the identical command after clearing the lock finishes the job. Also folded a pre-existing double-unregister in the "both local and global index exist" path into the same single call. Four regression tests cover the failing-delete, successful-delete, `--keep-config`, and global-only-entry cases.

### Added

- **CI now checks that every PR into `develop` touches `CHANGELOG.md`.** Added after several PRs (#193, #196/#197) landed with no changelog entry and nobody could later tell which bugs a given release actually fixed. The check (`.github/workflows/changelog-check.yml`) is visible-not-blocking — the same `--admin` merge override that bypasses this repo's review ruleset also bypasses a required status check, so making it required would add ceremony without enforcement; instead a missing entry is a red X on the PR and in `gh pr checks`, and skipping it deliberately requires labeling the PR `no-changelog` (for genuinely user-invisible CI/tooling churn). The diff is taken from the merge base so a rebase or develop-merge into the branch cannot false-pass the check with develop's own changelog commits. Also in this PR: env-mutating tests are now `#[serial]` with panic-safe restore (`crate::testing::EnvRestore`), and the `index rm` regression tests pin `CODESEARCH_SERVE_PORT` to an in-test reset-server so their serve-delegation probe can never fire a live `DELETE` at a developer's running serve.

## [1.2.10] - 2026-08-12

### Fixed

- **Caller-supplied MSYS POSIX paths (`/c/Users/...`) no longer silently create junk `<drive>:\c\Users\...` directories on Windows (#196, #197).** When an agent (or any non-MSYS caller — CLI, MCP client, `codesearch serve --register`) passed a POSIX-style drive path like `/c/Users/foo`, Rust on Windows resolved the leading `/` as "rooted on the *current drive*", i.e. `<current-drive>:\c\Users\foo`, creating orphan directories like `C:\c\Users\...` and silently indexing the wrong project. This is the path-pollution defect behind the orphan `<repo>-propagate-tmp` indexes (diagnosed end-to-end: an agent-created staging folder was indexed under both a real path and a polluted `C:\c\...` mirror that nothing ever cleaned up). Two new helpers in `src/cache/file_meta.rs`: `translate_msys_path` (Windows-only rewrite of `/c/...` → `C:/...` for a single ASCII letter after a leading `/` followed by `/` or EOL; idempotent on every other input; no-op on non-Windows where `/c/...` is a legitimate absolute path) and `normalize_user_path` (composes `translate_msys_path` + `strip_unc_prefix` — the single helper for every `safe_canonicalize(...).unwrap_or_else(_)` fallback site, per the repo's "structural fix" rule for the warnings-channel defect class). `safe_canonicalize` itself now calls `translate_msys_path` *before* canonicalising, so the success path is also covered structurally — not just the fallback. Applied at every user-supplied path boundary (11 production sites): `db_discovery/repos.rs` (`register`, `register_with_alias`, `unregister_path`, `alias_for_path`, `scan_for_remote`), `db_discovery/mod.rs` (`resolve_database_with_message`), `index/mod.rs` (the three `try_delegate_*_to_serve` functions + both `normalize_for_cmp` closures), and `serve/mod.rs` (`run_serve`'s `--register` loop). Non-repo indexing stays supported — intentionally NO git-repo check was added; the defect was purely about path resolution. Comprehensive regression tests pin both branches (existing-path success path + non-existing-path fallback, the actual defect site), plus register/unregister symmetry, plus Unix no-op guard.

- **A repo that failed to open once stayed broken until `serve` was restarted — a cached conflict is no longer replayed forever.** When opening a repo's database failed — typically a transient write lock, e.g. an indexing run holding the DB at the moment a query arrived — `ServeState` cached `RepoState::Conflicted`, and the fast path in `get_or_open_stores` replayed that error on every later call without ever retrying the open. The state's only documented exit was idle eviction, and that exit was unreachable: `evict_idle_repos` iterates `last_access`, but both paths that mark a repo Conflicted (`warmup_repo` and the `get_or_open_stores` slow path) propagate the failure with `?` *before* reaching their `touch_access` call, so a conflicted repo never gets a `last_access` entry and is never considered for eviction — however long it sits idle. Querying it did not help either: the fast path replayed the cached error while calling `touch_access` on the way, so the only queries that would have registered the repo for eviction were also the ones resetting its idle timer. Net effect: a momentary lock became indistinguishable from permanent corruption, curable only by restarting serve, while the error text promised the opposite ("the next query will retry automatically"). A cached conflict is now dropped on the next access and the open genuinely retried — cheap when it still fails, since that is just a refused file lock. This mirrors the missing-DB path, which already refused to cache `Conflicted` for the same reason. Regression test asserts recovery *without* a restart or an idle wait, and was confirmed to fail before the fix.
- **A federated peer is now never polled on a timer — the two 1.2.0 "scale-to-zero" fixes did not actually stop the cloud peer being woken.** 1.2.0 replaced the TUI's hardcoded 30s peer poll with the local serve's own `idle_suspend_secs` cadence and suppressed the startup poke, on the theory that polling no faster than the host's suspend term is harmless. It is not, and the release notes above overstated the fix. Measured on the deployed Azure Container Apps peer over a period with **zero** federated searches: wakes exactly **120/121/120 minutes** apart, each warm period **~67 min** — roughly a 50% duty cycle on an index nobody queried, each wake additionally paying an `azcopy sync` of the docs blob and a KB `git pull`. Two independent defects combined. **(1) The trigger:** the poll *itself* was the ingress traffic that woke the replica. Not keeping a peer awake *past* its suspend term is strictly weaker than not *waking* it, and the two windows were unrelated values anyway — the cadence read the **local** host's 2h default, not the peer's ~1h (which is why the 120-minute spacing, not 60, is the tell). **(2) The amplifier:** the cloud keep-warm loop fell back to the process start time when no tool call was recorded (`most_recent_tool_call().unwrap_or(start)`), and since `/status` and `/healthz` never call `record_tool_call`, any non-tool-call wake made the replica self-ping every 120s for its whole idle window — ~11× amplification. That fallback was unreachable in the case it was written for: a real tool call always records itself, so it could only ever fire when the wake was *not* real work. Now: the TUI's discovery tick is **config-only** (5s, zero HTTP) and merely rebuilds mounted-remote rows from the `remote_mounts` allowlist so mount/unmount edits and `l` reloads still surface; a peer is contacted only by an **activity poke** (a real federated search/get_chunk just hit that peer, so it is demonstrably already awake — single-peer, never a fan-out) or the explicit `i` info-overlay keypress. Keep-warm requires a real recorded tool call and otherwise lets the host suspend the replica. A peer staying warm for an hour *after real use* is correct and unchanged. Idle mounts render activity as `-`, now the normal steady state rather than a fault. Also fixed: removing the *last* peer from `repos.json` left its rows on screen forever (the snapshot was gated on a non-empty peer list), and the 1.2.0 "keep-warm target isn't self" warning false-positived on the only deployment where keep-warm is correct (the process binds `0.0.0.0` while the target is the ingress FQDN — a wildcard bind means the external host is unknown, so the check now stays silent). Background polling of **local** repos is unchanged and unaffected: the local/federated split is a deliberate design constraint.
- **`MDB_MAP_FULL` fatal crash on large corpora — LMDB mapsize cap raised + persistent embedding cache now auto-resizes too (#189).** Indexing a large corpus (e.g. a 1GB / 53k-file cargo-registry source producing >1.2M chunks) could crash with `MDB_MAP_FULL: Environment mapsize limit reached` once the vector store's auto-resize (already in place since an earlier fix) hit its old 8GB hard cap. Two changes: (1) the cap is raised to 16GB by default, and made runtime-overridable via `CODESEARCH_MAX_LMDB_MAP_SIZE_MB` (clamped to at least 1GB) for corpora that legitimately need more; (2) the **persistent embedding cache** (`~/.codesearch/embedding_cache/<model>/`) previously had no resize logic at all — it hit the same `MDB_MAP_FULL` on a hardcoded 512MB cap and silently degraded to a WARN-and-continue path, turning every subsequent embedding into a full ONNX-inference cache miss. It now retries with the same doubling-resize pattern as the vector store (up to 3 attempts, capped at the same runtime limit), persisting the grown size to `metadata.json` so a restart reopens at the correct size. When either store's cap is genuinely exhausted, the error/warning message now names the env var that raises it, instead of just reporting the size.
- **`build.ps1` now self-heals `core.bare=false` before invoking cargo.** This repo lives at `codesearch.git` as a bare+working-tree hybrid — a full checked-out source tree + `.git/index`, but `core.bare=true` in `.git/config`. `core.bare` intermittently resets to `true` (VS Code's git integration rewrites `.git/config` on ref changes; smoking gun: `github-pr-owner-number` duplicated 7× for `develop`), and when it does, cargo's source fingerprinting aborts every build with `did not expect repo ...\.git to be bare`, breaking `copy-to-common.ps1` → `build.ps1` → `cargo build`. `build.ps1` now forces `core.bare=false` right after `Set-Location`, before any cargo invocation. Idempotent and harmless for a normal (truly non-bare) checkout; non-fatal if git is unreachable.

## [1.2.0] - 2026-08-03

**TypeScript & Protobuf indexing, remote-TUI auth, cloud + cancellation hardening.** First minor bump since the 1.1.0 federation release: TypeScript joins `find_impact` via SCIP, Protobuf is now a tree-sitter-indexed language, the standalone remote TUI works against authenticated serves, and the embedded serve TUI stops waking scale-to-zero cloud peers — plus a cloud-serve OOM/read-only fix, honest index-cancellation, and a self-cleanup backstop for orphaned index dirs.

### Added

- **TypeScript SCIP symbol indexing for `find_impact` (#167).** `.ts` / `.tsx` / `.mts` / `.cts` files now get the same symbol-precise call-graph C# already had: `find_impact` returns file/line-accurate references for TypeScript symbols. A new `TypeScriptSymbolIndexer` (mirroring the C# adapter) drives Sourcegraph's `scip-typescript` via `npx` — a single-pass defs+refs write into LMDB, so `find_references` is a pure read with no subprocess. The file watcher debounces a TS rebuild on `.ts` changes and branch switches, and the serve TUI shows a TS symbol-index indicator next to the C# one. No binary is shipped in the release bundle: `npx` resolves `scip-typescript` on the host, and when `npx` is absent the indexer reports unavailable so MCP degrades gracefully to the lexical `find kind="usages"` fallback.
- **Protobuf (`.proto`) as a first-class indexed language — Niveau 1 (#162, #175).** `.proto` files are now parsed with [`tree-sitter-proto`](https://crates.io/crates/tree-sitter-proto) and chunked along `message` / `enum` / `service` / `rpc` boundaries instead of falling back to naive line-windowing. Definition chunks classify as Struct (`message`), Enum (`enum`), Interface (`service`), Method (`rpc`), and preceding `//` / `/* */` comments are captured as docstrings. This is text-aware indexing only — symbol-level precision (`find_impact` / call-graph for protobuf, "Niveau 2") is deferred until a motivating gRPC/Kafka-schema corpus exists, since there is no `scip-protobuf` emitter today.
- **Standalone remote TUI (`codesearch serve tui --url ...`) now works against authenticated remote serves (#182).** Previously it did an unauthenticated `/health` check with no way to pass a key, so it failed with 401 against any auth-required serve (e.g. the cloud peer). It now resolves the API key for the given URL from `repos.json` (`remotes.*.url` match) or a new `--api-key` CLI override, reusing the existing `build_serve_client_with_key` helper — the same `Authorization: Bearer` header the federation client already uses, so no new auth mechanism was invented. The authenticated client is passed through to all TUI actions (status/info/doctor/reindex/remove/reload), with clear, distinct error messages for "no key configured" vs. "key rejected (401)". No behavior change for local (non-authed) serves.

### Changed

- **Test-suite reorg (710 → ~604 tests, no coverage lost) (#180).** Extracted embedded `#[cfg(test)]` blocks out of bloated `mod.rs` files into sibling `_tests.rs` files (mcp/serve/search/cache/db_discovery); collapsed ~109 near-duplicate predicate tests into table-driven tests; centralized a repeated test helper (`state_with_repo`). Also closed 3 coverage gaps found during the pass: `repo_read_only` force-reindex refusal, a federation slow-peer → `Unreachable` timeout, and a `remove_repo`-during-active-build end-to-end race.
- **`codesearch remote available` / `index list --remote` now tolerate an unreachable peer (#164).** Both commands write-through-cache a peer's alias list on success and fall back to the last-known list (instead of hard-failing) when the peer is unreachable; `reconcile()` prunes cache entries for peers that no longer exist.

### Fixed

- **Cloud serve OOM crash-loop + read-only search regression (#177).** The federation peer's heavy DOCS corpus couldn't run inside a 1 vCPU / 2 GiB serve replica: write-mode warmup of six vendor repos peaked at 1.94 GiB and crashed (exit 137). Fixed with a per-repo `repo_read_only` flag — the indexer job builds write-mode then marks DOCS read-only before snapshotting; serve restores read-only and skips warmup entirely (0.1 GiB steady-state). Also fixes a latent LMDB bug this exposed: `open_readonly` opened DB handles inside a transaction it then `drop()`ped instead of `commit()`ted, so LMDB closed them and every read-only store returned a bare `EINVAL (os error 22)` on first use — shipped since the initial commit, only visible once read-only became a permanent code path. Ghost-vendor (vanished source) and dead-vendor (empty index) pruning so one bad vendor can't veto a snapshot publish. Structurally closes the "a store that fails mid-request renders as an ordinary empty/short result" defect class via `respond_with_items()` / `respond_with_object()` (the warnings channel is a required parameter, not an optional field), `qualify_empty_result()`, and a `#[must_use]` `MultiReadOutcome`; and enforces caller-facing literal line-continuation correctness via `tests/caller_facing_literals.rs`.
- **Index cancellation was a no-op for freshly-added repos; `remove_repo` reported "DB deleted" while the task kept writing (#178).** Diagnosed from a runaway `codesearch serve` (6 GB RSS, 40-52% CPU, machine unresponsive): `remove_repo`'s `CancellationToken` was never passed into the spawned task and the `JoinHandle` was never registered, so `cancel()` fired into the void and the DB dir was deleted under a still-writing task (Windows sharing violation → swallowed `warn!`). The token is now threaded through `force_reindex` / incremental refresh and checked inside the per-batch embed loop; `add_repo_handler` registers the handle so `remove_repo` actually cancels + awaits it; an early-bail guard prevents a removed alias being resurrected by its own in-flight task; and `remove_repo` now reports the DB-delete result honestly (`db_deleted: true|false` + reason). Test cache isolation also fixed — tests no longer write into the real `~/.codesearch/embedding_cache/`.
- **Orphaned `.codesearch.db` dirs left behind by cancelled in-build index tasks (#179).** The await-shutdown from #178 dropped the `JoinHandle` on its timeout — in Tokio this only **detaches** a task, it doesn't cancel it, and a task parked inside the synchronous arroy `build_index` (on a `spawn_blocking` thread) has no cancellation point. So the detached task held its LMDB handle open and the `.codesearch.db` dir stayed undeletable after removal. Added a self-cleanup backstop: the detached uninterruptible-build task drops its LMDB handle (closing the env synchronously) and deletes the orphaned dir right after releasing it — wired into all six build paths (add / reindex-force / TUI reindex post-build, FSW-refresh, primary FSW warmup, incremental-reindex). The delete is deadline-bounded (60s) and retries only on lock-class errors; already-gone is treated as success.
- **Embedded serve TUI polled a federated peer's `/status` every 30s regardless of its scale-to-zero configuration.** This defeated Azure Container Apps scale-to-0 for the cloud peer, since the background polling itself was enough ingress traffic to keep the replica perpetually warm. The TUI now polls a mounted peer at the serve's own configured `idle_suspend_secs` cadence (1h on the cloud deploy) instead of a hardcoded interval. Federated peer activity in the TUI now renders as `-` when stale (>5min since the last successful poll) rather than showing a misleadingly-fresh value, and a new `remote_peer_activity` map in `ServeState` triggers an immediate, event-driven refresh of the specific peer whenever the operator performs a federated search/get_chunk — so activity is never more stale than the operator's own last interaction. Local (non-federated) repos are entirely unaffected. *(Superseded in 1.2.4 — this cadence still woke the peer; see below.)*
- **Embedded serve TUI poked each federated peer's `/status` once on startup.** The scale-to-zero cadence fix above still left the discovery task firing its first poll immediately on startup (poll-then-sleep), so simply restarting the local serve pinged every federated peer once just to fill the dashboard — waking the cloud peer for no real reason. The first discovery cycle now builds the remote-project rows from config alone (no HTTP) and ships them with an empty refresh-time map, so every federated peer renders as stale `-` immediately; the first real `/status` refresh comes only from either the hourly cadence tick or an activity poke (a real federated tool call). Local repos are entirely unaffected. *(Superseded in 1.2.4 — see below.)*
- **Watcher-triggered reindexes were invisible in the serve TUI, and branch switches never rebuilt symbols.** Three related gaps in the `codesearch serve` file watcher: (1) the ordinary text-batch reindex (the most common watcher activity) never signalled the TUI, so editing a file showed nothing in the status column even though the index updated — despite the callback's own doc claiming it fired on "batch flushes"; (2) a C# symbol rebuild toggled only the general repo-state label, never the C#-specific indicator, so that column never showed "Indexing" during the (30–90s) rebuild; (3) a git **branch switch** refreshed only the text index and discarded the buffered `.cs`/`.ts` events without rebuilding symbols, leaving `find_impact` serving references from the previous branch until the next incidental `.cs` edit or a serve restart. Now: the text-batch flush toggles the TUI "Indexing" label; the C# notifier is a 3-state signal (`Started`/`Succeeded`/`Failed`) so the C# indicator shows "Indexing" for the rebuild duration; and a branch switch triggers a full C#/TypeScript symbol rebuild. Watcher symbol-rebuild log lines now carry the repo label for multi-repo attribution.
- **`model: unknown` on indexes created via the serve / git-hook path (git worktrees especially).** When a repo was registered through `POST /repos` (the git-hook flow), the vector store was opened first and `ensure_schema_version` pre-created a `metadata.json` containing only `schema_version` — no model fields. The force-reindex path then saw the file already existed and skipped stamping the default model, so the index was left with no `model_short_name`. Every reader reported `model: unknown`, and that sentinel disabled the empty-index live-chunk-count self-heal, making a perfectly good worktree index look empty so agents fell back to grep. The serve/git-hook and incremental-refresh paths now always stamp the resolved model. As part of the fix, the model→metadata stamp (`model_short_name`/`model_name`/`dimensions`) is consolidated into a single `ModelType::write_metadata_fields` source of truth across all five index-creation sites — which also corrects a pre-existing drift where the auto-create-DB path wrote the Debug variant name (e.g. `AllMiniLML6V2Q`) as `model_name` instead of the real model name. Existing worktree indexes need one reindex to pick up the stamped model.
- **Flaky `force_reindex_stamps_model_when_metadata_has_only_schema_version` test on Windows under parallel `cargo test`.** `atomic_write_json`'s `fs::rename(&tmp_path, path)` could race a Windows AV/Search-Indexer handle hold on the destination file, failing with `Access is denied (os error 5)` under parallel test execution. Added `is_transient_rename_error()` (classifies raw OS errors 5/32/33 — ACCESS_DENIED/SHARING_VIOLATION/LOCK_VIOLATION — plus a message-hint fallback, mirroring the existing `ServeState::is_db_locked_error` pattern) and wrapped the rename in a bounded retry (up to 5 attempts, 20ms backoff) for transient errors only. Validated with `cargo test --lib --bins` across 6 runs (default and `--test-threads=32`), all green.
- **claude-code grep-guard hook leaked `grep` on every low-confidence codesearch result.** The hook blocked the first `Grep` on an indexed repo path but auto-unblocked the *same* query when retried within 5 minutes — intended as the "codesearch found nothing, fall back to grep" path. But a low-confidence or empty codesearch result is a *successful* call meaning "reformulate the query", not a dead server, so the retry-cache let `grep` through whenever a query merely scored below the relevance floor (e.g. punctuation-heavy or alternation patterns). Replaced the retry-cache with an active liveness probe: the hook now GETs the serve hub's unauthenticated `/healthz` endpoint (base URL from `CODESEARCH_SERVER`, else `127.0.0.1:$CODESEARCH_SERVE_PORT`, else the compiled default `:39725`) and keeps `grep` blocked whenever the server answers, allowing it only when the probe fails — i.e. codesearch is genuinely down. Both the PowerShell and bash hooks are updated (the bash hook now also requires `curl`), and the deny message steers to `find`/`explore`/single-clean-term reformulation instead of promising an auto-unblock.

## [1.1.31] - 2026-07-23
- Security hardening sweep (Aikido): path-traversal fixes in `index` + `scip-csharp`, ANSI/control-sequence injection stripped from indexed content, `.git`/`node_modules` rejected as project roots, Unix path-cache key collision; `rmcp` 1.5.0 → 1.8.0 (3 CVEs) plus ~100 transitive dependency bumps. Also added EmbeddingGemma retrieval support (#155), `CODESEARCH_ALLOWED_HOSTS` / `CODESEARCH_DISABLE_HOST_VALIDATION` (#149), and `raise_fd_limit()` at serve startup (#150); fixed a multi-byte UTF-8 panic in search snippets (#148).

## [1.1.30] - 2026-07-10
- Added a user-configurable extension→language map (#138) at `~/.codesearch/extensions.json` (or `$CODESEARCH_EXTENSION_MAP`), letting a codebase opt in a non-standard extension (the reported case: legacy PHP in `*.inc`); user entries take precedence over the built-in extension table.

## [1.1.29] - 2026-07-10
- **Project-level federation + cloud reindex hardening.** Opt-in `remote_mounts` allowlist with `codesearch remote available|mount|unmount|mounts`; a mounted project is addressable as `project=<peer>/<alias>` and `@peer` group fan-out is restricted to mounts; TUI renders mounts with a Remote Mount info panel. Cloud indexer rebuilt as one sequential federated project per vendor (fixes the OOM-kill on reindex), image now built with BuildKit. Fixed `hooks git install` from worktrees (`core.hooksPath`, hook chaining, msys path) and `filter_path` returning zero results on federated/mounted *and* serve-routed local projects.

## [1.1.0] - 2026-07-01
- **Federation release.** Remote peer search fan-out (`search`/`get_chunk` over TLS, RRF-merged, never hard-fails), `--remote <peer>` index management (`list/add/rm/reindex`), split cloud indexer/serve topology, README `## Security` section; fixed `active_sessions` overflow to `u64::MAX`, `index rm <alias>` OS-path fallback bug, added `ls` alias.

## [1.0.212] - 2026-06-21
- Added reserved virtual `all` group (#131, always resolves to every registered repo); improved MCP agent discoverability instructions (#130, `INSTRUCTIONS_TEMPLATE` + README "Agent Guidance"); fixed `index add/rm/reindex` missing `CODESEARCH_SERVE_API_KEY` header on delegated serve requests (#132).

## [1.0.209] - 2026-06-17
- Fixed repos stuck showing "Indexing" forever in the TUI: `active_reindexes` `DashSet` leaked entries on task panic/cancellation. Replaced with a self-healing `DashMap<String, Instant>` that lazily evicts stale entries (`CODESEARCH_MAX_INDEXING_SECS`, default 30 min).

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
