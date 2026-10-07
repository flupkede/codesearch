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

## [1.5.3]

### Fixed

- **Group fan-out hits resolve in their origin store, with the alias-prefixed path.** Chunk ids are per-repo counters, so across a group every repo answers for id 0 and any two non-empty repos collide. The fan-out helpers deduplicated by (alias, chunk_id) but flattened to bare results, so every downstream resolver probed all stores and kept the first get_chunk(id) hit — the alphabetically-first repo stole the hit, the true definition died in the KINDS filter, unrelated content rode the stolen score, and group paths never picked up their alias prefix. MultiReadOutcome now carries SourcedResult{alias, result} from the helpers themselves: find definition/usages, literal search (BM25 and scan paths), group semantic in all three modes ((alias, chunk_id) interned to unique synthetic ids for the bare-u32 RRF fusion), graph imports/dependents/similar, explore outline dedup and get_chunk auto-routing all resolve the chunk only in its origin store and prefix the path from that store's alias; merge_exact_into_fts gains a (alias, chunk_id)-keyed sibling for the group lexical path.
- **Mixed-model groups warn that scores are not comparable.** A group whose repos embed with different models answers with cosine distances from different vector spaces, which the merged ranking presents as one scale — an agent reading the top hit's score as "strongest match overall" is comparing coordinates from unrelated spaces. The group path now collects the distinct models that actually answered and appends a warning naming every one of them; RRF fusion in auto/hybrid partially hides the problem by fusing ranks only, so semantic_mode's raw cosine scores are where it bites hardest.
- **Literal search relaxes to OR when the exact AND query matches nothing.** Business queries arrive as sentences, and a chunk holding most of their terms is a better answer than silence. When the conjunctive pass returns nothing and the query carries at least two significant (non-stopword) terms, the search retries as a disjunction and keeps candidates covering 60% of those terms (minimum two); coverage counts terms in the stored searchable_text (signature + docstring + content — the same fields the relaxed pass queries and rescues by), with a bare-content fallback for legacy blobs, so a candidate rescued BY its signature is no longer dropped by a gate that only read its content. The response flags relaxed_fallback and explains the mode in a note. Single-term queries never relax, so an empty answer for an unknown identifier stays an honest refusal.
- **All responses emit 1-based line numbers.** Chunks carry 0-indexed start/end lines by design and every consumer emitted them raw — a symbol defined on the first line of a file showed start_line 0. The conversion happens exactly once at each final emit boundary (semantic funnel, literal match lines, find, explore outline, get_chunk, similar chunks, imports, CLI renderer); intermediate vectordb results, store metadata, chunk-internal context slicing and SCIP-based positions stay untouched, and the Chunk doc comment pins the convention so the offset cannot be applied twice.
- **get_chunk reads surrounding context from the owning repo's root.** Chunk paths are stored project-relative, and the group-scoped answer rewrites them with the alias prefix before the context read — which then joined the prefixed path onto the serve process's own project root, a location that never exists. Every group-mode get_chunk with context_lines therefore answered "source file not readable, returning indexed content only", always. The context read now resolves the pre-prefix stored path against the owning alias's root from the routing context (`alias_roots`), keeping the historical join only for stdio single-repo mode where it was already correct.
- **Optional min_score threshold for semantic search, with an explicit refusal.** Nearest-neighbour search always answers something, so junk queries got confidently scored garbage and negative probes looked like weak hits. Hits below the opt-in threshold are dropped in the single funnel every local mode shares, and an answer emptied by the filter carries a note naming the threshold instead of reading as "no match in the index"; scores are mode-specific (cosine similarity for semantic_mode="semantic", RRF points for auto/hybrid/lexical) and the parameter docs spell out both scales. Federated group/project queries now forward the threshold to every leg and surface each leg's refusal note in `warnings` (the local leg hardcoded None and the peer body omitted the field, so both silently ignored it); literal mode has no calibratable scale and refuses the parameter by name instead of silently dropping it. Default behaviour is unchanged.
- **Debounced config persist no longer clobbers racing registrations.** The persist worker cloned its config snapshot and wrote repos.json without the config lock, so a POST /repos landing in that window existed only in memory while the stale snapshot hit disk — every reader-side reload_if_changed then trusted the fresh mtime as an external edit and swapped the disk copy in, silently dropping the registration (alias 404s in /info while its build ended with "repo removed or cancelled mid-index"). persist_config now adopts its own write's mtime so an internal persist never masquerades as an external edit, and the persist pass re-verifies memory against the snapshot after each write, rewriting until the file converges.
- **DB directories that survive the delete budget are quarantined.** When remove_repo's delete retries expired with the directory still held by an external process, it stayed in place as a fully valid index: the next registration adopted its stale metadata.json as false readiness, group fan-outs served an empty or stale store, and re-registration against legacy data ended in arroy EINVAL. The survivor is renamed to a .codesearch.db.removed-<unix_ms> sibling instead (atomic on unix; live holders keep their fds and exit with them), so a fresh registration builds from scratch; deferred cleanups owned by a running index task are excluded, and the DELETE response reports status "removed_db_quarantined" with a db_quarantined field so the caller sees the honest outcome. serve also sweeps the `.removed-*` remnants on startup (best-effort delete, failures warn): the sibling was left "for manual cleanup", and manual cleanup never happened — every budget-expired delete leaked a full stale index copy forever.
- **The db_discovery verbatim-path registration test is platform-independent.** register() stores the canonical form by contract, and on macOS the tempdir sits behind the /var -> /private/var TMPDIR symlink, so the byte-exact comparison against the raw path was testing an OS quirk rather than the invariant and always failed there. The input is pre-canonicalized (safe_canonicalize is idempotent on a canonical path), so any remaining byte difference is a genuine rewrite.
- **Crash-atomic file metadata with .bak recovery.** file_meta.json was written with plain truncate-and-write on the FSW update path that saves after every indexed file — a wide window for an unclean shutdown to leave a zero-byte or torn file. The pilot hit exactly this: warmup marked every file of a repo Changed (323/323 files fully re-embedded) or failed the incremental refresh outright, and a corrupt file never self-heals, so warmup kept skipping the repo until the file was deleted by hand. save now keeps one best-effort .bak generation, writes a pid+counter-unique temp sibling (the refresh task and FSW updates may save the same repo concurrently), fsyncs and renames with a bounded retry; load_or_create falls back to the .bak generation with a warning, and only when no generation parses does it degrade to an empty store — letting the callers' guard clear the vector/FTS stores deliberately and re-index instead of poisoning warmup with a permanent hard error.
- **codesearch setup --model persists the choice and serve adopts it.** setup downloaded the model and silently forgot it, so a bare codesearch serve fell back to the built-in default for every new index and the operator re-typed --model embeddinggemma-q4 on every serve launch. The choice persists to <codesearch_home>/default_model.json (tmp+fsync+rename) and serve adopts it when its own --model flag is absent (the flag still wins); serve also runs an offline pre-flight probe of the effective default (flag > persisted > built-in) against the hf-hub cache layout fastembed downloads into, warning up front with the setup hint instead of surfacing the absence mid-warmup as parallel index jobs hitting the network behind a proxy.
- **Managed post-checkout hooks are regenerated on serve startup.** codesearch hooks git install was the only writer, so blocks written by older binaries stayed stale in every repo forever — the pilot kept 515 of them, and the only upgrade path was re-running the install command per repo by hand. serve now refreshes codesearch-managed blocks for all registered repos in a background spawn_blocking pass at startup: an outdated managed region is rewritten to this build's block in place (idempotent), repos with no hook or a foreign hook are left untouched, failures warn and never block startup.
- **--global index stores live under CODESEARCH_HOME with collision-proof slugs.** They leaked into a hardcoded ~/.codesearch.dbs that ignored the CODESEARCH_HOME relocation knob every other global artifact honours, and named each store directory by the bare project file_name — two same-named projects at different paths silently shared one store. New stores go to <codesearch_home>/dbs/<name>-<path-digest>, with the historic layout still winning when it already exists so old indexes stay reachable; get_global_db_path no longer registers the project in repos.json (discovery and serve only ever look at <project>/.codesearch.db, so a running serve built a second, empty local store while the global one sat unused), and rm detects a real global store by existence and actually deletes its files instead of printing Global-index-removed over surviving data.
- **Per-model embedding init locks and a bounded model-load timeout (CODESEARCH_MODEL_LOAD_TIMEOUT_SECS).** A cold-cache model resolves through hf-hub, and on networks that black-hole the model host that fetch never completes and never errors; the old get() held the services map lock across the whole load, so one wedged fetch poisoned every embedding query — the whole-night serve stall. The map lock now covers lookups only, loading runs under a per-model init lock (double-checked after acquisition), and the load is bounded (default 600 s), failing fast with a hint to pre-populate the cache via codesearch setup; on timeout the loader thread is leaked deliberately — it holds no pool locks. Search callers get get_if_cached(), a pure lookup that never initialises.
- **Interactive fan-outs try-lock past busy stores instead of queueing behind indexing.** Group fan-outs waited up to STORE_LOCK_WAIT_SECS (300 s) per busy store and waits sum — a handful of repos held by an indexing run stalled every other repo answer. Every interactive multi-repo fan-out now try-locks each store through one shared helper and skips busy repos with a warning naming them: semantic search in all modes, find (definition/usages, imports, dependents), explore similar, get_chunk's candidate scan, and the literal BM25/tokenless metadata reads; single-project queries keep the bounded wait — there is no fan-out to wedge behind. A repo whose model was not loaded turned the first semantic query into a network fetch of model files, unbounded on a proxied network, holding the map lock and poisoning unrelated queries: search now accepts only loaded models or models provably on disk (is_model_in_cache) — groups skip such repos with one warning per distinct model (FTS fusion still covers them in hybrid/auto/lexical), a lone project fails fast with a setup hint, and the no-download gate guards the serve/MCP query paths (the CLI search command manages its own model loading). embed_query now runs ONNX inference in spawn_blocking instead of pinning the async worker for the whole pass.
- **Idle MCP sessions are reaped after 30 minutes.** keep_alive=None assumed TCP liveness would reap dead clients, but a wedged request keeps its socket and its session worker alive indefinitely (observed active_sessions=4 with a single client, each leaked session pinning an FIFO-serialized rmcp task). 30 minutes of idle is far beyond any local interactive pause yet guarantees a stuck session — and its queued requests — eventually goes away without a serve restart.
- **Empty stores answer queries instead of erroring.** An indexing run that ends with zero embeddable files never builds the arroy tree, and search() treated the missing graph as a hard error — a group fan-out reported each of the pilot's 31 empty repos as a per-repo failure on every semantic query. A chunkless store now returns an empty result (last-chunk probe: chunks present without a graph is a genuinely broken state and stays a loud error), so empty repos read as "no hits there", like they always did on the FTS path.
- **Visible shutdown notice after q.** q is the only graceful-exit trigger — no SIGINT/SIGTERM handler exists, and while the TUI holds raw mode Ctrl-C is not even delivered as a signal. After q, axum waits out open MCP SSE sessions (long-lived, they never close on their own) until the 3 s forced-exit deadline, which from the restored terminal reads as a hung process; the natural reflex — a second Ctrl-C, delivered once raw mode is off — hard-kills serve mid-drain. The TUI now prints right after restoring the terminal how many sessions are being drained and that the process exits by itself, so the pause is visibly bounded.
- **Ignore-sensitive tests are hermetic against a developer's global .codesearchignore.** The walker and watcher tests walk tempdirs and assert exact file sets and matcher presence, but FileWalker/FileWatcher also load the machine's global ignore — so a developer who actually customizes that file got permanent red local runs (the pilot's list starts with *.bin, which pruned test.bin before the binary check ever ran, and any global file at all broke the no-matcher assertion) while CI, with a clean home, stayed green. Both test families now pin CODESEARCH_HOME and its HOME/USERPROFILE fallback to fresh empty directories via the crate's #[serial] + EnvRestore idiom.
- **Serve no longer parks 9 tantivy OS threads per warm repo (~4.5k resident threads on a 500-repo pool).** Every open store held an eager IndexWriter — 3 indexing workers sized from the CPU count, a segment updater and a 4-thread merge pool that NoMergePolicy never schedules — plus a reader-side meta.json poller waking ~2x/s, all idle forever in a search-only pool while observability tools (ps -M, sample) choked on the thread census. The FTS writer is lazy again: it materializes only on the first actual write; readers of self-owned stores switch to ReloadPolicy::Manual because every commit path already reloads its own reader explicitly, and only genuinely externally-written readonly stores keep the OnCommitWithDelay watcher; a created writer now runs one worker and one merge thread instead of tantivy's derived defaults. Cross-process locking is unchanged (the codesearch-level .writer.lock still guards the database), and this process's own commits stay immediately visible to its searches.

## [1.5.2] - 2026-10-05

### Fixed

- **Restore runs only when package assets are missing or stale, and warning-severity workspace diagnostics no longer count as index warnings.** Restoring on every reindex was far too heavy for multi-repo hubs and big solutions legitimately exceed any fixed timeout (`dotnet restore did not finish within 300s` was observed live). Now the helper checks each project's `obj/project.assets.json` against its .csproj and restores only when evidence demands it (the steady state of an actively developed repo is a no-op), the kill timeout is tunable via `CODESEARCH_RESTORE_TIMEOUT_SECS`, and restore uses `--ignore-failed-sources` so an unreachable private feed cannot block indexing. Separately, warning-severity workspace diagnostics (e.g. "Found project reference without a matching metadata reference" for excluded or external projects) are benign resolution notes — they log at INFO and no longer pin the repo's index warning; only true load failures (minus NuGet audit advisories) still do.
- **C# symbol indexing restores packages before the workspace load, and NuGet audit advisories no longer count as index warnings.** The design-time load does not restore: with stale or missing `project.assets.json`, package-typed references collapsed into CS0246 cascades that flagged whole solution families as degraded (⚠ in the serve TUI) even though `dotnet build` succeeds. The helper now runs `dotnet restore` before opening the solution (bounded, best effort — a failed restore logs and continues, degraded indexing beats no indexing), and when the bounded wait expires the restore is **not killed**: it keeps running in the background, this load proceeds degraded, and the next reindex picks up the fresh assets — a killed restore would waste all of its work and re-pay the same wait on every subsequent reindex. NuGet vulnerability advisories surfacing as Msbuild "failures" are filtered from the warning channel: they are security notes for the product, not index-health signals. Compiler *warnings* were already never counted; genuine project compilation errors still are, and indexing stays as complete as the broken projects allow.

### Changed

- **Dependency refresh:** libc 0.2.190, rmcp 3.5.0, uuid 1.27.0 (#305).

## [1.5.1] - 2026-10-04

### Added

- **`codesearch setup` — download embedding models, and a relocatable global root via `CODESEARCH_HOME`.** The stubbed setup command now resolves `--model` through the shared model parser (unknown names error with the valid list), downloads into the global models cache with a visible progress bar (runtime paths stay silent), and probe-embeds one query so a broken download fails in setup instead of mid-indexing. `CODESEARCH_HOME` relocates the entire global root (repos.json, models cache, logs, serve_url, global ignore/extensions) and is honoured by the hub, db-discovery, every Claude Code guard hook and the OpenCode plugin, with tests pinning each layer. *(Contributed by @belovaf in #298 — in the same PR: find-impact warnings that no longer promise rebuilds nothing will run, group-fan-out hits attributed to their repo, and dependency-lockfile indexing exclusions.)*

*A big thank-you to belovaf ([@belovaf](https://github.com/belovaf)) for his first contribution to this release — a careful, well-tested feature plus four quality fixes landed in one go.*

### Fixed

- **Queued reindexes are no longer cancelled as "leaked", and an unjoinable index task can no longer wedge a hub.** With the process-wide job gate (`CODESEARCH_INDEX_JOBS=1`, the default), tasks queued behind a long-running job held their `active_reindexes` markers past `MAX_INDEXING_SECS` without renewing them, so the lazy stale-marker check evicted the markers and cancelled the parked tasks — leaving empty stores and in-process LMDB handles that made later writes fail with "Database is locked by another process" until a serve restart. Separately, `await_index_task` removed the task entry *before* waiting and dropped the `JoinHandle` on timeout, so a task too deep in the uninterruptible `build_index` to cancel kept its `Arc<SharedStores>` and the single job permit invisibly; the job-gate wait ignored cancellation, parking cancelled tasks on the gate; and `remove_repo` / format recovery could delete the DB directory out from under a still-running task. Now: a marker backed by a live tracked `Reindex` task is renewed (with a throttled wedge warning) instead of reaped; gate acquisition is cancellation-aware (`JobAcquire`) so cancelled or removed queued jobs abort and release their stores; a task that cannot be joined stays tracked and is reaped by a periodic sweeper that also deletes orphaned DB directories and re-asserts cancellation for removed repos; DB deletion is deferred while the task runs; format recovery refuses to wipe in that state; and stale markers from other owners are still evicted.

## [1.5.0] - 2026-10-03

### Added

- **Settable indexing/refresh limits: `CODESEARCH_INDEX_JOBS`, `CODESEARCH_EMBED_THREADS`, `CODESEARCH_EMBED_PAUSE_MS`, `CODESEARCH_MIN_FREE_MB`, `CODESEARCH_MAX_CHUNKS_PER_BATCH` (serve flags `--index-jobs`, `--embed-threads`, `--embed-pause-ms`, `--min-free-mb`).** A multi-repo hub used to start one full refresh/reindex/HNSW-build pass per repo the moment that repo had work, so a mass `git pull`, a burst of first queries or several HTTP reindexes ran many CPU-heavy passes at once, held many file/chunk windows in memory, and queued the (already serialised) ONNX inference behind all of them. A process-wide job gate now bounds concurrent heavy repo jobs (default **1**; queries never take it), the ONNX session's intra-op thread count can be capped (`CODESEARCH_EMBED_THREADS`, previously always all cores), background embedding releases the model lock once per mini-batch with an optional duty-cycle pause so interactive query embeddings are not stuck behind a whole batch, the read/chunk window can be capped by chunks as well as files, and a job can wait for a free-memory floor before starting. See the README section "Indexing limits" for recommended values.
- **OpenCode plugin integration (`integrations/opencode/`).** codesearch's MCP `initialize` instructions are advisory and connection-scoped: they cannot resolve the session directory to a registered project, see a `grep` return nothing, recover a dead serve hub, notice an unregistered worktree, or keep file outlines alive across compaction. The new self-contained OpenCode v2 plugin (no runtime dependencies, fail-open throughout) adds host-side behaviour around the same MCP server: it resolves the working directory to a project alias and injects scope-aware guidance once per session, turns a zero-hit `grep`/`glob` into index hits, offers `nudge`/`prune`/`block` codesearch-first guards that deny only while the hub is confirmed reachable and the repo is registered, probes `/healthz` and asks OpenCode to reload failed MCP servers (with optional local `codesearch serve` auto-start), warns when a registered index is empty or model-less, notes just-edited files while the watcher catches up, captures `explore` outlines into the compaction prompt, and registers `/codesearch`, `/codesearch-status` and `/codesearch-index` commands, a `codesearch_scope` tool and a `codesearch` skill. Includes a config reference, install/usage docs and a mock-host smoke test.

*A big thank-you to Don Bowman ([@donbowman](https://github.com/donbowman)) for his outstanding, highly value-adding contributions to this release — the indexing-limits rework (#294) and the OpenCode plugin integration (#295), follow-up fixes included.*
- **Always-on symbol indexes (C# + TypeScript).** Symbol (SCIP) indexes are no longer a manual or incidental artifact: serve warmup, cold opens, file-watcher batches and reindexes each drive a per-language symbol pass wherever chunk indexes are built, so `find_impact` evidence exists without a separate rebuild step. Startup inspects the on-disk symbol indexes with a builder-version stamp and rebuilds those an older binary produced (degraded retry instead of a hard failure), so a upgraded serve never answers from stale symbol data.

### Changed

- **`codesearch serve` runs heavy repo jobs one at a time by default (`CODESEARCH_INDEX_JOBS=1`).** Warmup, cold opens, file-watcher batches and reindexes now queue behind a running refresh instead of all running at once; raise the value deliberately on machines with cores and memory to spare. Standalone `codesearch index` / `codesearch mcp` runs are unchanged (no gate is attached). `CODESEARCH_EMBED_THREADS` unset keeps the previous ONNX default (all cores), so single-repo index throughput is unchanged unless the variable is set.
- **`find_impact` is honest about partial evidence; the TUI says how healthy the symbol indexes are.** Index-level rebuild warnings (non-compiling C# projects, swallowed resolution failures, non-zero `scip-typescript` exits) ride the `find_impact` answer as an additive `warnings` array instead of vanishing into the log. The scip-csharp helper's workspace-load diagnostics are aggregated into a single warnings line and never escalate to errors during workspace load. The serve TUI's symbol-index indicators use severity language: a degraded C# index renders red (`C#⚠`) and the info line names the failures.
- **Cloud image: build locally, not via `az acr build`.** The Dockerfile's builder stage compiles the release binary and warms up the embedding model before tarring — memory/disk-heavy, and ACR Basic/Standard build agents reliably fail the model-tar layer after a ~25 min compile (confirmed twice). `integrations/cloud/README.md` now documents building with local Docker Desktop and `docker push` instead.

### Fixed

- **SCIP key-format 2.0 regression: simple-name extraction now strips parameters before segmenting.** Fully qualified symbol keys that carry parameter types (real C# methods do) segmented on the parameter list instead of the path segments, producing wrong simple names for the affected symbols.

## [1.4.11]

### Changed

- **Repo state `Closed` renamed to `Idle`; the aggregated hub status now reports `indexed: true` + `status: "ready"` as soon as any repo has an index on disk.** After the idle reaper evicted a repo's stores (30 min inactivity by default), the repo reported state `Closed`, and when ALL repos were evicted the hub-level `index_status` answered `indexed: false` / `idle` — which small-model MCP clients read as "index unavailable" and fell back to raw grep, even though every index was intact on disk and the next query would reopen it (first query slightly slower). The state is now `Idle` (wire string `"idle"`; a peer still sending legacy `"closed"` renders identically), and the aggregated status distinguishes three cases: any on-disk index → `indexed: true`, `"ready"`, message naming Open/Warm/Idle counts with the auto-reopen hint; repos registered but nothing on disk → `no_index`; no repos registered → `no_repos`. Per-project statuses, reaper timing and behavior are unchanged.

## [1.4.10] - 2026-09-26

### Changed

- **⚠️ MIGRATION — chunk paths are stored project-relative; every existing index re-embeds ONCE on first refresh with this binary.** Chunk metadata, tantivy documents and the file-meta store now key files by their path relative to the codebase root instead of the absolute path, making a built database portable across machines and mount points (the cloud snapshot only worked before because build path and serve path happened to be identical). Consequence, by design: an existing database's absolute-path entries no longer match the new relative keys, so the first incremental refresh sees every file as changed and re-chunks + re-embeds the whole corpus one time (expect a full-reindex-length CPU/embedding burst per repo; orphaned absolute-path entries are cleaned as deleted). After that one pass, incremental behaviour is identical to before — refreshes are again change-only. **Tooling impact:** paths RETURNED by search results, `stats` and the TUI change from absolute to `<alias>/<relative>`; paths SUPPLIED to tools still accept both forms — absolute needles are relativised internally (`normalize_tool_path`), so feeding a returned relative path or an old absolute path back into `chunks_for_file`/`get_chunk` keeps working. The SCIP/symbol index uses its own separate key space and is untouched.

### Added

- **`codesearch index add --local` — build in-process, never delegating to a running serve.** Deliberate local builds (snapshot assembly, CI, offline tooling) previously either raced the serve delegation or had to fake a dead port; `--local` skips delegation explicitly. Pair it with `CODESEARCH_REPOS_CONFIG` (existing override, honours load AND save) to keep build registrations out of a developer's live `~/.codesearch/repos.json` entirely — the docs-harvester's `index-local` pipeline does exactly that for strict hub separation.
- **Non-TTY index runs write a clean `build.log`.** When stdout is not a terminal (CI, piped runs), phase lines are also written ANSI-free to `<project>/.codesearch.db/build.log`; `--log-file` forces a location on any run.

### Fixed

- **Incremental refresh reuses the serve hub's embedding pool (todo #135).** The refresh path constructed its own fastembed pool alongside the serve pool, doubling model memory per repo and re-initialising ONNX on every refresh; it now shares the serve pool. Single-serve runs are unchanged.
- **`current_git_head` no longer leaks caller `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE`/`GIT_OBJECT_DIRECTORY` into the queried repo.** A `codesearch index` invocation from inside a git hook (QC gate) inherits the hook's git environment and answered with the HOOK's repo state instead of the target project's, producing wrong head SHAs and spurious rebuild decisions; the four variables are stripped before `git rev-parse`.
- **`prefix_path_with_alias` no longer mis-detects absolute paths on Linux.** It used `Path::has_root()`, which only recognizes a Windows drive-letter path (`C:/...`) as rooted when the binary itself runs on Windows; on the Linux CI runner an out-of-root Windows path was treated as relative and wrongly alias-prefixed. Replaced with a platform-independent textual check.

## [1.4.9] - 2026-09-23

### Changed

- **fastembed 6.1.0 → 7.0.1.** Major bump of the embedding backend. Upstream renamed the `InitOptions` type alias to `TextInitOptions`; `embed/embedder.rs` migrates to the new name. No behavior change: same model set, same `FASTEMBED_CACHE_DIR` handling, the CPU execution provider still uses the arena allocator.

- **rmcp 3.3.0 → 3.4.0.** MCP SDK minor bump. The deprecated `ServerInfo` alias is now `ServerConfig`; renamed in the serve hub (`mcp/mod.rs`) and the stdio proxy (`mcp/proxy.rs`). Handshake, session semantics and tool responses are unchanged.

- **CI actions refreshed: actions/checkout 4.3.1 → 7.0.1, github/codeql-action 3 → 4.** Workflow-only; no product code touched.

## [1.4.4]

### Fixed

- **A resident C# workspace can no longer answer `find_impact` from source that predates the latest change.** The resident Roslyn workspace pool (todo #115) is keyed only by solution path and reused across `find_refs` calls, with no tie to repo state beyond an idle TTL. A symbol rebuild advanced `index_head_sha` to the new commit, but the resident workspace kept answering from the compilation loaded before it — a newly extracted method's call site went missing from `find_impact` with `index_head_sha == current_head_sha` and no warning (found via todo #165's end-to-end test). `WorkspacePool::evict` now runs at the end of every rebuild, full or incremental; a per-solution generation counter closes the window where a spawn already in flight for that solution would otherwise still install stale. `scip_ref_cache` is now also cleared in full on every incremental rebuild (previously only full rebuilds did): the old selective invalidation could purge a cache entry whose *existing* reference list pointed at a changed file, but could never catch an unrelated cached symbol gaining a brand-new reference *from* that file — reading the new content is Roslyn's job, not the cache's, so the selective scheme was removed once the blanket clear subsumed it.

- **LMDB keys may now be ~1980 bytes instead of 511 — C# symbol rebuilds stop failing on their own signatures.** heed's `longer-keys` feature builds LMDB with `MDB_MAXKEYSIZE=0`, deriving the limit from the page size. The scip-csharp helper writes fully qualified parameter types into the SCIP key, so real methods reach 912 bytes (`MasterDataSyncService#EnhanceCountryProductAsync(System.Collections.Generic.List<global::…>)`); at 511 LMDB answered `MDB_BAD_VALSIZE`, which failed the entire rebuild for that repo and — until the classifier learned better — wiped and reindexed the whole database, on every serve start, for five repos at a time. A key that still exceeds the limit is now skipped with a counted warning instead of failing the rebuild: `find_impact` loses that one symbol, the other ~40 000 are indexed. Note: a build without the feature cannot read the long keys this one writes.

- **A repo is wiped at most once per process for "storage-format corruption".** `MDB_BAD_VALSIZE` was read as "data written by an older arroy/heed major" and answered with a full wipe + rebuild. On 2026-09-17 the same five C# repos were wiped twice in one day: the second failure hit a database this process had created hours earlier, so the error was written by the *current* binary (LMDB rejects an empty or >511-byte key), not by an old format. Each misdiagnosis cost ~20 minutes of reindex per repo and re-armed itself on the next symbol rebuild. A second format recovery for the same alias is now refused and reported as a writer bug, the raw LMDB error is logged instead of only the conclusion, and every SCIP `put` carries its table name plus the offending key's size so the next occurrence names its own cause.

- **The last MDB_MAP_FULL retry no longer fails in silence.** After two resize doublings the third attempt returned its error without a single log line, which is why a wedged insert looked exactly like a crash (a 3.4 GB database at 0 chunks, no error in the log). All three retry loops — insert, delete and index build — now log the final give-up with the map size and the batch size, and announce each retry. A failed attempt also hands back the chunk ids it consumed: the transaction aborted, so the retry no longer pushes the id space (and arroy's item range) further out on every round.

- **A leaked indexing task no longer keeps a repo's LMDB env and writer lock open forever.** Evicting a stale `active_reindexes` marker only corrected what the TUI and the reindex guard believed — the background task behind it kept running and kept the `Arc<SharedStores>` it captured, so the env and `.writer.lock` stayed held for the process lifetime. The repo then logged as idle and "DB closed" while every write (`reindex`, format recovery, `POST /repos`) failed with "Database is locked by another process", indefinitely: observed on a repo left at 0 chunks for two days after a format-recovery rebuild wedged. The staleness check now cancels the task's token (cooperatively — the handle is still never aborted), format recovery reports a cancelled rebuild as a failure instead of logging "rebuild complete" over an empty index, and idle eviction warns with the holder list when the env is still open after the repo was evicted. The post-build self-cleanup that deletes an orphaned `.codesearch.db` now keys on the alias actually being gone from the config instead of on a cancelled token — a cancelled token no longer implies "repo removed", so the old rule would have wiped a live repo's index whenever a rebuild outlived its 30-minute marker.

## [1.4.3] - 2026-09-17

### Security

- **Dependency refresh: h2, rustls, quinn-proto, zerovec-derive, moka.** A semver-safe `cargo update` lifts 126 packages within their existing requirements, clearing five open advisories without code changes: h2 0.4.15→0.4.19 (Aikido 41883526), rustls 0.23.42→0.23.45 (RUSTSEC-2026-0285), quinn-proto 0.11.16→0.11.18 (Aikido 41883527), zerovec-derive 0.11.3→0.11.6 (Aikido 41883531) and moka 0.12.15→0.12.16 (Aikido 41297220). fastembed/ort are deliberately kept at 5.17.3/rc.12 — 5.17.4 hard-requires the unstable ort rc.13 API break, which moves in its own PR.

- **rmcp 1.8.0 → 3.3.0 — clears the five Aikido advisories on the MCP SDK (34247111).** Behavior-identical upgrade: the legacy `initialize` handshake and session semantics stay the default (`ProtocolVersion::LATEST` remains 2025-11-25); the 2026-07-28 stateless lifecycle is opt-in upstream and deliberately not enabled here. Code impact stayed small because the `#[tool_router]`/`#[tool_handler]` macros absorb the new MRTR response enums: `Content`/`RawContent` are now `ContentBlock`, the constructors-only impl takes `#[tool_router(allow_empty)]`, the stdio proxy's manual `call_tool` returns `CallToolResponse`, and content assertions drop the removed `.raw` projection.

### Changed

- **ort rc.13 + fastembed 5.17.4 (ONNX runtime refresh).** fastembed 5.17.4 hard-requires ort 2.0.0-rc.13, so both move together. rc.13 renamed the CPU execution provider (`CPUExecutionProvider` → `ort::ep::CPU`); the embedder's import and construction follow, builder chain unchanged (arena allocator still on). Scope note: ort-sys rc.13 still pins `lzma-rust2 ^0.15`, so the lzma-rust2 0.16.5 advisory (Aikido 37515813) remains open until upstream bumps its build dependency.

- **Vector-index stack aligned on rand 0.10: arroy 0.5→0.8, heed 0.20→0.22.** The direct `rand` requirement moves to 0.10.2 — the major rmcp already uses — instead of pinning three rand majors side by side in the lock. arroy 0.8 and heed 0.22 follow because arroy 0.8 hard-requires both (`rand ^0.10.2`, `heed ^0.22.1`); no call-site changes were needed beyond silencing heed's `EnvFlags::NO_TLS` deprecation (same LMDB `MDB_NOTLS` flag, same behavior — the type-state `Env<WithoutTls>` migration is deferred). heed-types 0.21, roaring 0.11 and ordered-float 5 ride along transitively. Note: the rand 0.9.5 Aikido flagged rides the fastembed→hf-hub/tokenizers chain and clears with the fastembed 6 major, not with this change.

- **Embedding stack majors: fastembed 6.1, hf-hub 1.0, ndarray 0.17.** Zero call-site changes — the codesearch embedder sits on fastembed's `ModelType`/`InitOptions` surface, which 6.1 kept stable. Scope note: fastembed 6.1 still resolves `image 0.25`, so the weezl 0.1.12/moxcms 0.8.1 advisories (Aikido 30640676/37515815) remain open until fastembed adopts image 0.26+; likewise rand 0.9.5 stays via hf-hub 0.5/tokenizers.

- **tantivy 0.22 → 0.26 with graceful FTS reset.** The collector API changed so `TopDocs::with_limit` needs `.order_by_score()` at the three call sites. Because tantivy cannot open index files written by an older major, an unreadable FTS index is now wiped and recreated as a fresh empty index instead of failing the whole DB open — the FTS index is derived data, BM25 results rebuild on the next (re)index, and a warning is logged pointing at `codesearch index`. Vector search and all non-FTS paths are unaffected; pinned by a regression test that feeds the store a corrupt `meta.json`.

- **TUI stack majors: ratatui 0.30, crossterm 0.29.** Zero call-site changes — the serve TUI sits on stable surface (`CrosstermBackend`, `Terminal`, `TableState`, `Paragraph`, `Layout`). This removes the last lru 0.12.5 path (ratatui's chain now carries lru 0.18.4; tantivy stays on the 0.16.4 that upstream 0.26 pins).

- **axum 0.7 → 0.8.** The only breaking surface hit: path parameters changed syntax from `:param` to `{param}` — all route registrations (`/repos/:alias*`, `/chunk/:id`) and the shared `CHUNK_PATH` constant move to brace syntax, including the federation client's URL templating that derives from the same constant. Extractors, middleware and `axum::serve` compile unchanged; the serve + federation test suites exercise the rebuilt router end-to-end.

- **thiserror 1.0 → 2.0 (direct).** Drop-in for all error enums (`#[error]`, `#[from]` unchanged); tantivy's chain still carries thiserror 1.x transitively until upstream moves.

- **File-watch stack majors: notify 6.1 → 8.2, notify-debouncer-full 0.3 → 0.7.** The debouncer absorbed `Watcher` into `Debouncer` itself (`debouncer.watch()/unwatch()` replace `.watcher().watch()`), and root cache tracking is now automatic, so the explicit `cache().add_root()` call goes away. The watcher's cache type now follows upstream's per-platform recommendation (`RecommendedCache`: `FileIdMap` on Windows/macOS, `NoCache` on Linux — file-ID tracking is an internal rename-detection optimization; event mapping never reads file IDs directly). Also removes `mio 0.8.11` from the lock entirely (it rode the Linux-only inotify 0.9 path; notify 8 uses inotify 0.11).

- **tree-sitter 0.26 → 0.27, tree-sitter-proto 0.4 → 0.6.** Zero call-site changes — the chunker sits on the stable `Parser`/`Language` surface and grammars load through the ABI-stable `LANGUAGE.into()` (`tree-sitter-language`) route, so all 17 grammar crates stay pinned while the core moves a major.

- **Small majors batch: dirs 7, sha2 0.11, scip 0.10, sysinfo 0.39; dead `tower`/`tower-http` direct deps removed.** dirs/scip/sysinfo were drop-in. sha2 0.11's digest arrays no longer implement `LowerHex`, so the two hash-to-hex sites (`file_meta.rs`, `chunker/mod.rs`) hex-encode the digest bytes explicitly — output unchanged. `tower` and `tower-http` were declared as direct dependencies but never imported anywhere (CORS/trace middleware never wired in); removing them shrinks the direct dependency surface (both remain in the lock transitively via axum/reqwest/hf-hub, which is upstream's business).

### Fixed

- **Serve auto-recovers LMDB storage-format corruption with a sequential wipe + rebuild.** After the arroy 0.5→0.8 / heed 0.20→0.22 major upgrades, every repo whose on-disk database was written by the previous binary failed its symbol rebuild with `MDB_BAD_VALSIZE: Unsupported size of key/DB name/data, or wrong DUPFIXED size` (observed on all C# repos after deploy). When a symbol rebuild now fails with that error class, serve wipes the repo's DB directory — closing the LMDB envs first via the same eviction sequence `remove_repo` uses, with the same bounded retry for transient Windows lock holders — and force-reindexes it through the existing force-reindex machinery, whose store-open path recreates everything on the new formats. Recoveries are queued and processed strictly **one repo at a time**: each rebuild runs a full CPU-bound embed pass, so parallel recoveries would thrash the machine. Read-only repos are skipped with a pointer to the owning writer. This closes the gap the tantivy FTS graceful reset (above) already covered on the FTS side — the vector/symbol stores now self-heal the same upgrade boundary instead of staying red until an operator force-reindexes by hand.

### Fixed

- **Dependency batch: criterion 0.8, serial_test 4, indicatif 0.18, colored 3 (dependabot majors) + CI actions refresh.** The four cargo majors were drop-in for lib/bins; benches and dev-test surface absorbed the criterion 0.5→0.8 and serial_test 3→4 API moves. The five GitHub Actions bumps (upload-artifact v7, download-artifact v8, cache v6, setup-dotnet v6, action-gh-release v3) are the dependabot-proposed SHA pins. dependabot itself now targets develop permanently (`target-branch` in the default-branch config): its rebases used to reset the base to master, tripping the `check-source-branch` guard.

- **Federated chunk fetch works again — URL residue and project-scope routing (todo #153).** Two independent defects: (1) the peer URL for a `chunk_ref` fetch was built by replacing `{id` without the closing brace, so the constructed path carried a stray `}` (`/chunk/2058%7D`) that real peers answered with 400 Bad Request — axum's `{id}` parameter happily swallowed the stray brace into the captured value, which is exactly why the mock-based tests never caught it; the replacement now covers the full `{id}` placeholder, pinned by a test whose route echoes back the exact path it was hit on. (2) `get_chunk` with `project=<peer>/<alias>` plus a plain `chunk_id` died in local routing with "Unknown alias" — mounted remote projects are now routed through the same federated fetch search uses (local aliases still win a name clash), so both `chunk_ref` and `project=`+`chunk_id` forms work against remote peers.

## [1.3.19]

### Changed

- **`find_impact` never silently picks a symbol or an adapter.** An ambiguous name (overloads, multiple definitions on one line) now returns a structured ambiguity envelope with sorted candidates instead of the shortest fuzzy match; an explicit `symbol_key` request field selects an exact candidate (mutually exclusive with `symbol_name` / `file`+`line`), and every resolved answer names its canonical key in the new additive `resolved_symbol` field. With more than one language index installed and no `language` given, the tool asks which one instead of silently using the first (todo #139).

### Added

- **`find_impact` surfaces partial results.** Index warnings — non-compiling C# projects, swallowed reference-resolution exceptions, `scip-typescript` non-zero exits with usable output — now persist (C#: alongside the cached refs in the same transaction; TS: in the index meta) and ride the answer as an additive `warnings` array instead of vanishing into the log, so consumers such as the `audit` binary's `impact`/`removals` never read incomplete evidence as zero (todo #139).

- **Claude Code web-guard hook is topic/mount-scoped.** Only queries about the mounted products reach the web guard, and the retry cache is keyed per mount.

- **`codesearch serve --model X` sets the default model for newly created indexes.** Previously `--model` was inert on `serve` (each repo's query model comes from its own `metadata.json`); now it is the model a repo is indexed with when it is added through serve without an explicit model — `POST /repos`, including the `codesearch index add` path delegated to a running serve — so an operator can make a non-default model the norm without passing `--model` on every add. An index that already records its own model is never overridden (an explicit `model` in the request still wins). Serve reports the default in `GET /status` as `default_model` and at startup. It is deliberately **not** the query fallback for a repo whose `metadata.json` records no model: a legacy index built before the model-recording contract is queried with the built-in 384-dim default, and the search response carries a warning naming the assumed model and the re-index command. Following the serve default there would break a legacy repo the instant an operator set `--model` to a model of another dimension (a 384-dim index queried with a 768-dim model fails), and degrade it silently for a same-dimension model.

### Fixed

- **`status` no longer reports `ready` for an index that cannot be searched.** Readiness keyed only off `total_chunks`, so a repo mid-rebuild — chunks inserted, `build_index()` not yet run — reported `status: "ready"` / "Index is ready for searching." while every search failed with `Index not built. Call build_index() after inserting chunks.` Both the single-repo and group status paths now require the vector index (`stats.indexed`) to be built before reporting `ready`, otherwise they report `building` with a message that says the vector index is not built. A store that failed to report stats is still surfaced separately (degraded-ready with `warnings`), never misread as "not built".

- **Adding a repo with `--model` now creates the index at that model's dimension.** `POST /repos` — the path `codesearch index add --model …` delegates to when serve is running — opened the store with the default 384-dim dimension and applied the model override only to `metadata.json` afterwards. The background reindex then embedded 768-dim EmbeddingGemma vectors into a 384-dim store and indexed nothing: the `.codesearch.db` directory existed, `stats` showed the new dimension, and no files were indexed. The store is now opened at the override's dimension.

- **The CLI no longer downgrades a non-default index to the 384-dim default.** `codesearch index` resolved its embedding model as `--model`-or-`ModelType::default()` and never consulted the model recorded in the index's `metadata.json`, so re-indexing a repo built with EmbeddingGemma embedded 384-dim MiniLM vectors against a 768-dim store (and `FileMetaStore` logged "Model changed, full re-index required", wiping the file metadata). The CLI now resolves the recorded model through the same helper the serve/watcher paths use; an explicit `--model` that disagrees is rejected with a pointer to `--force`. Relatedly, `codesearch stats`, `get_db_stats` and the repo listing opened the vector store with a hardcoded 384, so `Dimensions:` always read 384 for every index — they now read the recorded dimensions.

- **`status(kind="index")` reports the routed repo's model, not the service default.** The `model` field was the service's own model (the hardcoded default in serve mode) while `dimensions` came from live store stats, so every repo read `minilm-l6-q` regardless of what it was indexed with. It now resolves per repo for `project=`, and reports the common model — or `mixed` — for a group.

- **Serve mode embeds each query with its target repo's indexed model.** The multi-repo MCP service built its shared embedder from `ModelType::default()` (384-dim MiniLM) and ignored both `--model` and the `model_short_name` each index records, so every semantic query against an index rebuilt with a 768-dim model (EmbeddingGemma) failed with `Query embedding dimension mismatch: expected 768, got 384` — and a same-dimension mismatch would have silently compared incomparable vector spaces. The service now resolves the model per routed repo (the same `metadata.json` contract the indexing path already followed) through a per-model `EmbeddingServicePool`; group fan-out embeds the query once per distinct model and searches each store with its own. (See the `serve --model` default for newly created indexes under **Added**.)

- **Persisted helper-exit warnings are platform-stable — Linux CI green again.** `ExitStatus`'s `Display` renders `exit code: N` on Windows but `exit status: N` on Unix, so the non-zero-exit warning the TypeScript symbol indexer persists into its meta table disagreed with its own regression test on Linux, failing `test-linux`/`csharp-integration-tests` deterministically since #238. A shared `exit_status_text()` renders `exit code: N` on every platform (signal-terminated processes fall back to the platform string), applied to the persisted TS warning and the C#/TS helper log lines, pinned by a cross-platform unit test.

- **C# canonical symbol keys no longer collapse distinct declarations.** Generic arity (``M`1``), the full containing-type chain and fully qualified parameter types are part of the key again, so overloads that previously shared one identity keep their own. The index version is bumped to 2.0 with a key-format stamp in the index meta: a stale-format index reports as absent and rebuilds once on upgrade (todo #139).

## [1.3.16]

### Added

- **REST `/find-impact` endpoint (HTTP mirror of the `find_impact` MCP tool).** The read-only REST surface (`/search`, `/find`, `/explore`, `/chunk/:id`) now also mirrors `find_impact`: POST a `FindImpactRequest` body (`symbol_name`, or `file`+`line`; optional `language`, `project`, `group`) and receive the tool's JSON payload — busy envelope and `index_head_sha`/`current_head_sha` freshness fields included. Same auth class as the other REST mirrors: open on localhost binds, bearer key on network binds. Lets non-MCP clients — notably the `audit` binary — consume SCIP reference evidence without an MCP session.

## [1.3.15]

### Added

- **`edit-guard` — a fourth Claude Code guard hook: edits now require a codesearch consultation first.** On `Edit`/`Write`/`MultiEdit` against a file in a codesearch-registered repo, the hook denies the edit unless codesearch was consulted for that exact path within the last 5 minutes: `find_impact` for SCIP-backed languages (`.cs .ts .tsx .mts .cts`), `find(kind="usages")` for everything else. Markers are recorded by `edit-guard-post`, the first Claude Code `PostToolUse` hook in this repo: it fires on every `find_impact` call and every `find(kind="usages")` (kind check done script-side — matchers only see tool names), counts any outcome ("no results" and "no SCIP backend" included, so the guard can never wedge permanently), and prunes expired entries on write. The guard accepts ANY marker for the path within the window and fails open on unregistered repos, non-git paths and a crashed hook; missing/corrupt state counts as not consulted (deny on covered repos, allow everywhere else). Shared target-resolution/coverage helpers moved into `codesearch-common.sh/.ps1` (grep-guard sources them too; its PowerShell twin is thereby ported off the last `.codesearch.db`/Windows-only-path coverage signals, closing the #199 gap). The native installer writes the new scripts plus a `PostToolUse` registration, idempotent by exact command as before; the subagent preamble gained an EDIT RULE line. Hook self-tests: `bash integrations/claude-code/hooks/run-tests.sh` (todo #134).

## [1.3.14]

### Fixed

- **Concurrent cold opens no longer wedge a repo behind the LMDB double-open guard.** Two overlapping first opens of the same repo (e.g. a `find_impact` racing its own retry) could both reach `try_open_stores`; the loser tripped the double-open guard and cached `Conflicted`, which the self-heal could never cure while the winner held its env — the repo stayed broken until a serve restart (2026-09-08 incident, todo #131). Cold opens are now single-flight per alias in `ServeState`: the loser waits on a per-repo lock and then hits the winner's cache entry. Covers both cold-open entry points (`get_or_open_stores`, `warmup_repo`); the fast path stays lock-free.

## [1.3.13]

### Fixed

- **scip-csharp stderr no longer scrambles the serve TUI.** The resident `scip-csharp serve` helper inherited the host's stderr, so MSBuild workspace diagnostics (e.g. project-load failures from stale NuGet restore state) were sprayed raw over the TUI, bypassing the file-only serve logger. Helper stderr is now piped and drained through tracing, with helper `[WARN]`/`[Failure]` lines classified as warn level across all invokers (index, find-refs, batch-find-refs, serve) — workspace-load errors land in the codesearch log and surface as warnings instead of corrupting the terminal.

## [1.3.10] - 2026-09-03

### Added

- **Resident SCIP helper for `find_impact` (C#).** `scip-csharp serve` loads the Roslyn workspace once and answers find-refs requests over a JSON-lines protocol; a `WorkspacePool` caps resident workspaces (default 2, per-workspace heap cap, idle teardown). `find_impact` consults the pool first and keeps the one-shot spawn as fallback.
- **Local MCP ownership and readiness controls.** `codesearch mcp --mode local --readonly` serves an index without taking the writer lock or starting refresh/file-watcher tasks; `--require-ready` refuses to start on a missing, empty, partial or otherwise unusable index. Both fail explicitly outside local mode, where they cannot govern a remote serve process.

### Changed

- **`find_impact` stays responsive and truthful on cold caches.** Lookups run under a configurable budget; on overrun a structured busy answer (`retry_after_seconds`) is returned while the lookup continues in the background, so a retry gets progress or the warm result. Failures are typed (`failed`/`stale`) with actionable hints, and results carry `index_head_sha` vs `current_head_sha` so index drift is surfaced instead of guessed at.
- **`find` kind=`usages` discloses its lexical nature.** Results are re-ordered code-first before the limit is applied, C#/TS hits carry a `note` naming the `find_impact` upgrade path, and the tool description states the caveat outright.
- **build.ps1 writes cargo output to a repo-local `.tmp/build-<mode>.log`** (printed afterwards) and warns — never kills — when cargo/rustc processes are already running.

### Fixed

- **Local stdio MCP no longer masks search errors with `LMDB double-open prevented`.** A failed read on the live shared store preserves the original error instead of attempting a second database open.
- **SCIP symbol index: concurrent queries and rebuilds no longer fail each other.** All SCIP opens share one LMDB environment per index directory, and error states are cleared on eviction/force-reindex so the TUI no longer shows a permanently frozen failure.
- **`index rm` on Windows (os error 32).** Dropping the tracked LMDB environment now actually closes the heed env, so `data.mdb`/`lock.mdb` are released immediately instead of staying locked for the process lifetime.

## [1.3.3] - 2026-08-18

### Added

- **Pre-commit hook enforces the root-md allowlist**: a commit adding a root-level `*.md` outside the allowlist is rejected with a pointer to `.docs/`. Stray root docs and the tracked `docs/` folder were dissolved into gitignored `.docs/`.

### Fixed

- **grep-guard resolves Grep coverage from the search target and serve-hub registration** instead of the hook's cwd and `.codesearch.db` presence: absolute POSIX paths are detected correctly, coverage follows `repos.json` registration (a nested unregistered clone counts as uncovered), and the resolver fails open.
- **`index rm` against a running serve completes the file delete without stopping serve.** The DELETE client gets its own derived 80s timeout, and the retry loop awaits in-process LMDB holder drain instead of backing off blindly.
- **`index rm` no longer claims "DB deleted" when serve could not delete the files** — a `db_deleted: false` response surfaces as a warning naming the leftover directory and the recovery path.

## [1.3.0] - 2026-08-15

### Added

- **`GET /indexing?path=...`** — per-repo freshness probe (`covered`/`indexing`), and the grep-guard hook now waits-and-retries while a branch-switch reindex is in flight instead of forcing a grep fallback. Repo resolution follows the grep target rather than the hook's cwd.
- **CI checks that every PR into `develop` touches `CHANGELOG.md`** (visible-not-blocking; deliberate skips require the `no-changelog` label). Env-mutating tests are now `#[serial]` with panic-safe restore.

### Fixed

- **Federated peers retry transient scale-to-zero responses (502/503/504, bounded)** inside the active tool call — a cold-starting peer surfaces as a short delay with a clear "retry in ~30s" hint instead of a raw non-JSON error. Non-transient statuses and transport errors are not retried.
- **`index rm` end-to-end acceptance path pinned by a real integration test**: CLI → health probe → DELETE → serve deletes the DB dir without being stopped → clean "Unknown alias" afterwards.

## [1.2.10] - 2026-08-12

### Fixed

- **MSYS POSIX paths (`/c/Users/...`) no longer create junk `<drive>:\c\Users\...` directories on Windows.** `translate_msys_path` + `normalize_user_path` are applied at every user-supplied path boundary; fixes the orphan `C:\c\...` index pollution.
- **A repo that failed to open once no longer stays broken until restart** — a cached `Conflicted` state is dropped on the next access and the open is genuinely retried.
- **Federated peers are truly never polled on a timer.** The 1.2.0 cadence fix still woke the cloud peer (the poll itself was ingress traffic, plus a keep-warm fallback amplified non-tool-call wakes). The TUI discovery tick is now config-only (zero HTTP); peers are contacted only by a real tool call or the explicit info overlay.
- **`MDB_MAP_FULL` on large corpora** — LMDB mapsize cap raised to 16GB (runtime-overridable), and the persistent embedding cache now auto-resizes instead of silently degrading to cache misses.
- **build.ps1 self-heals `core.bare=false`** before invoking cargo (VS Code's git integration intermittently flips the hybrid checkout bare, aborting every build).

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

[1.5.2]: https://github.com/flupkede/codesearch/compare/v1.5.1...v1.5.2

[1.5.1]: https://github.com/flupkede/codesearch/compare/v1.5.0...v1.5.1

[1.5.0]: https://github.com/flupkede/codesearch/compare/v1.4.9...v1.5.0

[1.4.9]: https://github.com/flupkede/codesearch/compare/v1.4.4...v1.4.9
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
