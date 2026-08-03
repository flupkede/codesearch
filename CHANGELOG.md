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

## [1.1.37] (unreleased)

### Added

- **Protobuf (`.proto`) as a first-class indexed language — Niveau 1 (#162).** `.proto` files are now parsed with [`tree-sitter-proto`](https://crates.io/crates/tree-sitter-proto) and chunked along `message` / `enum` / `service` / `rpc` boundaries instead of falling back to naive line-windowing. Definition chunks classify as Struct (`message`), Enum (`enum`), Interface (`service`), Method (`rpc`), and preceding `//` / `/* */` comments are captured as docstrings. This is text-aware indexing only — symbol-level precision (`find_impact` / call-graph for protobuf, "Niveau 2") is deferred until a motivating gRPC/Kafka-schema corpus exists, since there is no `scip-protobuf` emitter today.

### Fixed

- **Watcher-triggered reindexes were invisible in the serve TUI, and branch switches never rebuilt symbols.** Three related gaps in the `codesearch serve` file watcher: (1) the ordinary text-batch reindex (the most common watcher activity) never signalled the TUI, so editing a file showed nothing in the status column even though the index updated — despite the callback's own doc claiming it fired on "batch flushes"; (2) a C# symbol rebuild toggled only the general repo-state label, never the C#-specific indicator, so that column never showed "Indexing" during the (30–90s) rebuild; (3) a git **branch switch** refreshed only the text index and discarded the buffered `.cs`/`.ts` events without rebuilding symbols, leaving `find_impact` serving references from the previous branch until the next incidental `.cs` edit or a serve restart. Now: the text-batch flush toggles the TUI "Indexing" label; the C# notifier is a 3-state signal (`Started`/`Succeeded`/`Failed`) so the C# indicator shows "Indexing" for the rebuild duration; and a branch switch triggers a full C#/TypeScript symbol rebuild. Watcher symbol-rebuild log lines now carry the repo label for multi-repo attribution.
- **`model: unknown` on indexes created via the serve / git-hook path (git worktrees especially).** When a repo was registered through `POST /repos` (the git-hook flow), the vector store was opened first and `ensure_schema_version` pre-created a `metadata.json` containing only `schema_version` — no model fields. The force-reindex path then saw the file already existed and skipped stamping the default model, so the index was left with no `model_short_name`. Every reader reported `model: unknown`, and that sentinel disabled the empty-index live-chunk-count self-heal, making a perfectly good worktree index look empty so agents fell back to grep. The serve/git-hook and incremental-refresh paths now always stamp the resolved model. As part of the fix, the model→metadata stamp (`model_short_name`/`model_name`/`dimensions`) is consolidated into a single `ModelType::write_metadata_fields` source of truth across all five index-creation sites — which also corrects a pre-existing drift where the auto-create-DB path wrote the Debug variant name (e.g. `AllMiniLML6V2Q`) as `model_name` instead of the real model name. Existing worktree indexes need one reindex to pick up the stamped model.
- **claude-code grep-guard hook leaked `grep` on every low-confidence codesearch result.** The hook blocked the first `Grep` on an indexed repo path but auto-unblocked the *same* query when retried within 5 minutes — intended as the "codesearch found nothing, fall back to grep" path. But a low-confidence or empty codesearch result is a *successful* call meaning "reformulate the query", not a dead server, so the retry-cache let `grep` through whenever a query merely scored below the relevance floor (e.g. punctuation-heavy or alternation patterns). Replaced the retry-cache with an active liveness probe: the hook now GETs the serve hub's unauthenticated `/healthz` endpoint (base URL from `CODESEARCH_SERVER`, else `127.0.0.1:$CODESEARCH_SERVE_PORT`, else the compiled default `:39725`) and keeps `grep` blocked whenever the server answers, allowing it only when the probe fails — i.e. codesearch is genuinely down. Both the PowerShell and bash hooks are updated (the bash hook now also requires `curl`), and the deny message steers to `find`/`explore`/single-clean-term reformulation instead of promising an auto-unblock.

## [1.1.31] - 2026-07-23

**Security hardening sweep (Aikido) + community bug/dependency fixes.**

### Added

- **EmbeddingGemma retrieval support (#155, original work by @markschroedr, superseding #147).** Adds support for Google's EmbeddingGemma embedding model as an additional embedder option, alongside model-selection hardening and improved error messages for unsupported/misconfigured embedding models.
- **`CODESEARCH_ALLOWED_HOSTS` / `CODESEARCH_DISABLE_HOST_VALIDATION` (#149, reported by @stdweird).** rmcp's DNS-rebinding defence defaults the MCP transport's `Host`-header allowlist to loopback-only, rejecting container/service hostnames in containerised deployments. `CODESEARCH_ALLOWED_HOSTS` lets you extend the allowlist with a comma-separated hostname list; `CODESEARCH_DISABLE_HOST_VALIDATION=1` disables the check entirely (only safe behind a reverse proxy). See README `## Security`.
- **`raise_fd_limit()` at serve startup (#150, contributed by @tony-nexartis).** `codesearch serve`'s fd demand scales with registered repo count; under process supervisors with a low default `ulimit -n` (notably macOS launchd, 256), this could silently exhaust file descriptors and wedge `accept()` with `EMFILE` while the daemon still looked healthy. Serve now raises its own soft `RLIMIT_NOFILE` to the hard limit at startup (Unix only) and warns if the effective limit still looks insufficient for the repo count.
- **`persist-credentials: false`** added to every `actions/checkout` step across all GitHub Actions workflows, and the CodeQL workflow's floating `actions/checkout@v4` pinned to the same SHA already used elsewhere — reduces the blast radius of a compromised CI step and closes a supply-chain drift gap.
- **CodeQL skipped on fork PRs.** Fork-originated PRs carry a restricted `GITHUB_TOKEN` that cannot upload SARIF results to the upstream repo, which was failing the CodeQL check on every external contribution (e.g. #150) with a confusing "Resource not accessible by integration" error unrelated to the PR's actual code. The analyze job is now skipped for fork PRs (still runs on `develop`/`master` push, same-repo PRs, and the schedule).

### Fixed

- **Panic on multi-byte UTF-8 boundary in search snippets (#148, reported by @tony-nexartis).** Search-result snippet truncation byte-sliced content at a fixed offset, panicking whenever that offset landed inside a multi-byte character (box-drawing glyphs, CJK, emoji). Now truncates on a char boundary.
- **Path-traversal hardening (critical).** `codesearch index`'s project-path resolution no longer silently falls back to the raw, unvalidated path when canonicalization fails — it now fails fast with an actionable error. The `.NET` symbol-helper CLI (`scip-csharp`) now canonicalizes every path argument (`--solution`, `--project`, `--output`, `--symbols-file`) before use, closing several path-traversal vectors flagged by Aikido SAST.
- **Registering a `.git`/build-artifact directory as a project root.** `codesearch index`/repo registration now rejects a root whose own directory name matches an always-excluded name (`.git`, `.svn`, `node_modules`, etc.), preventing accidental indexing and search-exposure of internal VCS metadata.
- **ANSI/control-sequence injection in terminal output.** Search results and sync/reindex logs now strip ANSI escape sequences (CSI, OSC, Fe) and stray control characters from indexed file content before printing, so a maliciously crafted file can no longer manipulate the user's terminal (clear screen, hide output, rewrite the title bar, etc.).
- **Unix path-cache key collision.** The path-normalization cache used for file metadata unconditionally converted `\` to `/`, which on Unix (where `\` is a legal filename character, not a separator) could collapse a literal-backslash filename with an unrelated subdirectory path into the same cache key. The conversion is now gated to Windows only.
- **Dependency CVE remediation.** `rmcp` floor bumped `1.5.0 → 1.8.0` (3 CVEs fixed); ~100 transitive dependencies refreshed via `cargo update`, including security-relevant bumps to `quinn-proto`, `h2`, `hyper`, `tokio`, `rustls`, `openssl`, `zerocopy`, `zeroize`, `webpki-roots`, `aws-lc-rs`.

## [1.1.30] - 2026-07-10

### Added

- **User-configurable extension→language map (#138).** A new optional `~/.codesearch/extensions.json` (or the path in `$CODESEARCH_EXTENSION_MAP`) maps a file extension to a language name, e.g. `{ "inc": "php", "h": "cpp" }`. Files with an unrecognised extension are `Unknown` and skipped **entirely** during indexing (there is no line-based fallback for `Unknown`), so a codebase using a non-standard convention — the reported case is legacy PHP in `*.class.inc` files — was previously invisible to codesearch. The map lets users opt in per codebase; entries take precedence over the built-in extension table (so a known extension can be remapped too). Kept **generic on purpose**: `.inc` is not hardcoded to PHP because it's language-agnostic (assembly, SQL, C/PHP includes). Missing/malformed maps and unknown language names are logged and ignored, never fatal.

## [1.1.29] - 2026-07-10

**Project-level federation + cloud reindex hardening.** Builds on the 1.1.0 federation release: a peer's individual projects can now be **opt-in mounted** and queried by name, the serve TUI surfaces and inspects those mounts, and the cloud indexer was reworked to reindex reliably without OOM-killing itself.

### Added

- **Opt-in mounting of individual remote projects.** After adding a peer, the local user **explicitly picks** which of its individual projects to use, via a new `remote_mounts` allowlist in `repos.json` — nothing is auto-exposed. A mounted project is queried locally by name as `project=<peer>/<alias>` (e.g. `cloud/akeneo`), a 1-to-1 passthrough routed directly to that peer; a **non-mounted** project is unroutable even if the peer exposes it. The allowlist is the single source of truth for routing, discoverability, TUI display, and group fan-out.
- **`codesearch remote available|mount|unmount|mounts`.** Inspect the individual projects a peer exposes (marking which are mounted), then opt in/out. `remote available <peer>` queries the peer's `GET /status`; `mount`/`unmount` edit the allowlist; `mounts` lists the current selection (and any local rename).
- **Mounts are discoverable.** `list_projects` gains a `remote_projects` array (name + peer + peer URL), and the `scope_required` error advertises mounted names in `available_projects`, so an agent can find and route to a mounted project as a first-class `project=` target.
- **Group fan-out restricted to mounts.** A whole-peer `@peer` group reference (e.g. `docs → [@cloud]`) now federates only the individual indexes you mounted for that peer — each queried as its own project — instead of the peer's entire corpus.
- **TUI: mounted remote projects.** Mounts render in **italic/cyan** in the serve status table to signal they live on a peer (not a local index). The `i` (info) key now works on a mount, opening a **Remote Mount** panel showing the peer URL and the peer-reported live status (status / lock / changes / calls / last call). The panel also fetches the peer's on-disk index stats (**chunks / files / db size / model**) on demand from `GET /repos/{alias}/info`, giving remote mounts parity with the local Info overlay — with a loading placeholder while the fetch is in flight and a graceful "stats unavailable from peer" fallback if the peer can't answer. When a mount is selected, the footer renders the local-index actions **doctor / reindex / remove struck-through (disabled)** so it's clear those don't apply to a peer-hosted index; info / reload / quit / navigation stay enabled.
- **KB near-instant propagation.** The custom-KB project now polls its remote `git` HEAD on a cheap `git ls-remote` interval (`KB_POLL_INTERVAL_SECS`) instead of waiting for the full reindex cadence, so a KB add/update/delete becomes visible to federated queries within seconds of the git push rather than up to ~15 minutes later.

### Changed

- **Remote mount selection is opt-in.** Replaced the earlier auto-discover-everything / opt-out `remote_hidden` filter with the explicit `remote_mounts` allowlist. Live peer discovery now only **enriches** TUI status; it no longer defines which projects are mounted (mounts resolve from config even while a peer is unreachable).
- **Cloud indexer job: one federated project per vendor.** The cloud indexer now builds each vendor as a separate federated project (`akeneo`, `vendor-a`, `bynder`, `digizuite`, `inriver`, `keyshot`, plus the custom KB) rather than one monolithic index, and builds them **sequentially** so the serve replica only ever holds one embedding model in memory at a time.
- **Cloud deployment docs** generalised for public release (customer identifiers scrubbed) and consolidated under `integrations/cloud/`.
- **Docker image** now built locally with **BuildKit** (`docker buildx --push`) instead of `az acr build`: the model-cache warmup is folded into the builder stage and shipped as a single tarball, working around ACR's classic builder failing to `COPY --from` a chained stage / symlink tree.

### Fixed

- **`codesearch hooks git install` now works from worktrees, honours `core.hooksPath`, and chains into existing hooks.** The generated `post-checkout` hook registered the checked-out worktree with `codesearch serve` using `$(pwd)`, which on Git Bash is an msys path (`/c/…`) that serve rejects with HTTP 400 ("cannot canonicalize") — so worktree auto-registration silently no-op'd on Windows. The hook now sends `$(pwd -W 2>/dev/null || pwd)` (native `C:/…` on Git Bash, plain `pwd` elsewhere). Install-time fixes: the hooks directory is resolved via `git rev-parse --git-path hooks` so it (a) writes to the shared **common-dir** hooks when run inside a linked worktree — git never runs a per-worktree gitdir hook, so the old behaviour installed a hook that never fired — and (b) honours a `core.hooksPath` override. Instead of refusing when a foreign `post-checkout` already exists, install now **chains** a delimited codesearch block into it (inserted before any trailing `exit 0`) and upgrades that block in place on re-run, so it is idempotent. The managed block is POSIX `sh` (valid when chained into a `#!/bin/sh` hook) and JSON-escapes the path.
- **Indexer job OOM-kill on reindex.** The container entrypoint submitted all vendor index builds at once (async HTTP 202), so the serve process held every vendor's embedding model + working set simultaneously and got OOM-killed on 8 GiB — leaving the job stuck "indexing" forever. Builds now run sequentially, waiting for each to settle before starting the next.
- **Incremental-refresh OOM crash-loop.** Bounded incremental-refresh embedding batches so a large change set no longer exhausts the heap.
- **claude-code grep-guard hook** now ignores an already-running codesearch process and requires a local index before nudging toward codesearch, so it stops blocking `grep` when codesearch can't actually serve the current repo.
- **`filter_path` on federated/mounted projects returned zero results.** `search(project="<peer>/<alias>", filter_path=...)` (and `@peer` group fan-out) forwarded `filter_path` to the peer, which matched it against its own **un-namespaced** store paths (and, in serve mode, against the wrong project root) — so it dropped every hit regardless of the value passed, while the caller only ever sees the `<peer>/<alias>/…` **namespaced** path. `filter_path` is now applied **client-side** on the namespaced result paths for both the project-passthrough and group fan-out paths (the hub over-fetches from the peer and post-filters), so a federated `filter_path` matches exactly what the caller reads back. Consumers no longer need the over-fetch+post-filter workaround.
- **`filter_path` on a serve-routed local project returned zero results.** For a `search(project="<local-alias>")` (or local group) served by `codesearch serve`, `build_semantic_response` relativised result paths against the **service's own `project_path`** rather than the **routed project's root**, so the absolute stored path never stripped and every hit was filtered out. The filter now resolves the correct root per result (routed alias's root; the longest matching alias root for multi/group; the service path only as the stdio fallback), so `filter_path` behaves as a **repo-relative** prefix in every routing mode. stdio single-repo behaviour is unchanged.

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
