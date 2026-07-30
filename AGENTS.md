# AGENTS.md — codesearch (features/remote-mount-selection)

_Last updated: 2026-07-29_

## Current state

- **Version:** `Major.Minor.Patch` (semver). Patch auto-bumps +1 on every PR merged to `develop` (CI via `.github/workflows/bump-develop.yml`); minor bumps manually at release (`scripts/bump-version.sh --type minor`, resets patch→0). Per-commit uniqueness comes from `build.rs`'s `+<commit_count>` suffix. See `RELEASING.md`.
- **Validation:** `cargo check` for iteration, `cargo clippy -D warnings` for lint, `cargo test --lib --bins` before a branch is considered done. No `--release` builds during the fix loop — build only at the very end.
- **Deploy:** cloud peer runs the per-vendor federation split (one index per vendor sub-folder + custom-kb), image built locally via BuildKit `docker buildx --push`, all vendors reindexed and federation validated end-to-end (`project=cloud/<vendor>`).

## Implemented Features

- **Opt-in remote mount selection** (commit `1a5b3fc`) — a peer's individual projects are no longer auto-exposed; the user explicitly `remote mount`s the ones to use. `remote_mounts` allowlist in `repos.json` is the single source of truth for routing (`resolve_remote_project`), discoverability (`list_projects`/`scope_required`), TUI display, and `@peer` group fan-out (restricted to mounted projects only, never the whole peer). CLI: `codesearch remote available|mount|unmount|mounts`.
- **Remote project mounting (1-to-1 passthrough)** (branch `features/codesearch-federation`, merged) — each project a peer exposes is addressable locally as `project=<peer>/<alias>`, same as a local project. TUI renders mounts in italic/cyan with an info panel (peer URL + live status) and disables doctor/reindex/remove (those act on a local index a mount doesn't have). `FederationClient::search_project` forwards a single-project query directly to the peer. Cloud indexer job builds one index per vendor sub-folder sequentially (avoids holding every vendor's embedding model in memory at once — see OOM fix below).
- **Federation peers** — `codesearch remote add/rm/list` (local `repos.json` peer config: `alias → url, api_key, group, into_group`) + `@peer` group references; `FederationClient` search/get_chunk fan-out with RRF.
- **Cloud indexer-job split** — heavy 4 vCPU/8 GiB build job uploads a snapshot; light 1 vCPU/2 GiB serve restores it (DOCS corpus read-only). The serve replica additionally runs a **memory-bounded incremental reindex of the small custom-kb repo** after each KB `git pull` moves `HEAD` (fire-and-forget `POST /repos/custom-kb/reindex`), so new KB articles are searchable without a redeploy; the heavy DOCS corpus stays job-only. The DOCS-read-only state is now **enforced** via a per-repo `repo_read_only` flag in `repos.json` (set by the index job's `mark_docs_readonly` step): serve's warmup opens DOCS repos read-only and returns early — no embedding on the serve replica, so 1 vCPU / 2 GiB fits comfortably; only `custom-kb` stays writable. The index job also prunes ghost vendors (vanished source) before publishing the snapshot. Cloud peer live + validated. See `integrations/cloud/README.md`.
- **Remote index management (`--remote`)** — `--remote <peer>` flag on `index list/add/rm` + `index reindex` verb drives a peer's management API via `FederationClient` (`ManagementOutcome`: `Ok` / `HttpError{status,reason}` / `Unreachable`). Endpoints: `GET /status`, `POST /repos {path}`, `DELETE /repos/:alias`, `POST /repos/:alias/reindex[?force=]`. `--json` on List/Reindex (requires `--remote`). Without `--remote`, every `index` verb is unchanged (local).
- **Local `index rm <alias>`** — resolves the argument as a registered alias before falling back to path interpretation.
- **CLI aliases** — `ls` is a visible alias for `list` (`index`/`groups`/`remote`); `rm` for `remove` (pre-existing).
- **Protobuf (`.proto`) language support — Niveau 1** (#162, PR #175) — `.proto` files parsed with `tree-sitter-proto` and chunked along `message`/`enum`/`service`/`rpc` boundaries (Struct/Enum/Interface/Method) with preceding `//`/`/* */` comments as docstrings, instead of naive line-windowing. Symbol-level `find_impact` (Niveau 2) deferred — no `scip-protobuf` emitter exists today.

> ℹ️ **Remote write verbs** (`add`, `reindex --force`) require a read-write peer; the cloud peer rejects them (`--force` → HTTP 500 "could only be opened read-only; cannot force-reindex"). An **incremental** `reindex` (no `--force`) of an already-registered repo *does* succeed on the cloud peer — that is the custom-kb auto-refresh path. `list` is always safe. `rm` is not durable — the next cold start re-registers from the restored snapshot.

## Open TODOs

Single source of truth for outstanding codesearch work. Items marked 🔒 live in a separate worktree — **do not touch on this branch**.

### Code — small, ready to pick up

- [x] **T1: Remove dead `wait_until_indexed()`** in `docker/entrypoint.sh` — superseded by `wait_active_build_done()`. Confirmed no callers anywhere in the repo (only 3 comment references). Deleted the function + updated the comments.
- [x] **T2: Extract shared `build_remote_search_body(request, mode, limit_value)`** in `src/mcp/mod.rs` — group fan-out (`federated_search`) and single-project fan-out (`federated_project_search`) duplicated the same `serde_json` body (differing only in the limit value); extracted to one shared builder.
- [x] **T3: Persist remote-project discovery** to `remote_project_cache` in `repos.json` — the field already existed but was never read/written. Wired `ReposConfig::cache_remote_projects()`/`cached_remote_project_aliases()`; both `codesearch remote available <peer>` and `codesearch index list --remote <peer>` now write-through-cache a peer's alias list on success and fall back to the last-known list (instead of hard-failing) when the peer is unreachable. `reconcile()` prunes cache entries for peers that no longer exist. Shared the mounted/cached row printing into `print_remote_project_row()` to keep the two CLI commands in sync.
- [x] ~~**T4: 0-chunk status bug**~~ — **closed as can't-reproduce.** Static trace of the full call-graph found no concrete defect (fresh LMDB read-txn per `stats()`, no `Arc` swap, no stale handle); the `total_chunks==0 → "building"` inference at `src/mcp/mod.rs:7557`/`:7618` only fires in the genuine 0-chunk window or an unconfirmed narrow cold-start/concurrent-reload race — not reproducible, not biting in steady state. Re-file with a deterministic live repro if the symptom recurs.
- [x] ~~TUI `i`/`d`/`f` diagnostics~~ — investigated, this was a stale reference in the TODO title, not a code bug. Actual TUI keybindings (`src/serve/tui_common.rs`: `handle_key` + `render_footer`) are `i` (info), `d` (doctor), `n` (reindex), `r` (remove), `l` (reload), `q` (quit) — footer hints match the handler exactly. No `f` binding exists or ever existed in the codebase; the title's "f" doesn't correspond to anything real.

### Code — 🔒 separate worktrees (resolved)

- [x] ~~🔒 **find_impact routing diagnose/fix**~~ — **resolved via PR #163** (merged 2026-07-27, Option D = nudges/reframe: recommend find_impact first; stop deflecting to `find kind=usages`; align rustdoc; auto-detect TS SCIP extensions). Diagnosis doc kept in repo root as `DIAGNOSE_FIND_IMPACT_ROUTING.md`.
- [x] ~~🔒 **TypeScript SCIP indexing**~~ — **resolved via PR #167** (merge `98a1979`, 2026-07-28). SCIP protobuf parsing, `TypeScriptSymbolIndexer` + registry wiring, file-watcher TS tracking, tests+fixture+smoke, TUI indicator, Windows `npx` fix. Plan doc kept as `PLAN_TYPESCRIPT_SCIP.md`. Follow-up SCIP-adapter dedup tracked as T5.

### Cloud / infra — needs decision before pickup

- [ ] **C1: Automate the manual `codesearch-indexer` trigger** — currently `triggerType: "Manual"`; every rebuild today is a human running `az containerapp job start` by hand. The 2026-07-04 batching fix (see "Historical context" below) means large batches can no longer crash anything, but staleness is still only resolved manually. Options, not yet decided (needs vendor content update-cadence info):
  - **Schedule trigger** on the existing job (`az containerapp job update --trigger-type Schedule --cron-expression "..."`) — no new Azure resources, just a cron cadence.
  - **Event-driven** (Event Grid on the blob source triggering job start) — more precise, needs a new Event Grid subscription + small trigger function/Logic App.
  - The single-app redesign (C2) remains a separate, bigger follow-up.
- [ ] **C2: Single-app collapse redesign** (collapse indexer job + serve into one scalable app) — proposed design ready:
  1. `az containerapp update -n codesearch-serve --cpu 2.0 --memory 4Gi` — new revision, cold start (restore last snapshot, sync corpus, start incremental reindex in-process).
  2. Poll `GET /status` every ~10-15s (timeout e.g. 15 min) until **all repos report `"status": "warm"`** — replaces the fragile in-process `indexing`-flag/120s-timeout detection in `entrypoint.sh` that caused the 2026-07-04 crash.
  3. Once warm, trigger a snapshot upload (existing `upload_snapshot` logic).
  4. `az containerapp update -n codesearch-serve --cpu 1.0 --memory 2Gi` — new revision, cold start, restore-only.

  **Why the blob round-trip is unavoidable:** LMDB (mmap-based) is not safe on network-mounted volumes (Azure Files/NFS — mmap needs local POSIX byte-range locking a network share can't reliably provide). The index must live on local ephemeral disk, and ephemeral disk does not survive a Container Apps revision change (any `--cpu`/`--memory` update triggers one) — hence some durable handoff (blob snapshot) is structurally required.

  **Scoped first step shipped (2026-07-08):** incremental reindex in-process on serve is live — but *only* for the small **custom-kb** repo (`docker/entrypoint.sh`'s serve-mode KB pull loop fires `POST /repos/custom-kb/reindex` whenever `git pull` moves `HEAD`). Safe on the 1-2 GiB replica because incremental refresh is memory-bounded. The heavy DOCS corpus deliberately stays job-only.

  **Still open:** retire `codesearch-indexer` entirely or keep for DR; scheduled script vs Logic App vs wrapper CLI command (`codesearch cloud rebuild --remote <peer>`?).

### GitHub issues

- [~] **#162: include protobuf as a language aware** — Niveau 1 (text-aware `tree-sitter-proto` chunking on `message`/`enum`/`service`/`rpc` boundaries) shipped in PR #175. Niveau 2 (SCIP symbols → `find_impact`/call-graph) deferred pending a `.proto`-heavy repo — no `scip-protobuf` emitter exists today.
- [x] **#161: missing macOS binary in v1.1.31** — fixed: C1/C3/C4 (APFS disk-pressure retry: stage binary out of `target/` + `cargo clean` + tar/cp retry loops with `df -h` diagnostics) merged via #166; PR #173 pinned the `actions/checkout` `ref:` so `workflow_dispatch` builds the tagged commit (related mismatch class). GitHub issue #161 closed 2026-07-29.

### Defensive / low priority

- [x] **D1: Apply same cp-retry pattern to Linux `with-csharp` step** in `release.yml` — the "Package with-csharp (Linux)" step now retries the binary `cp` up to 3x with `df -h` diagnostics on failure and a hard `test -f` check, mirroring the macOS step's C3 pattern. Preventive consistency only (Linux runner has 84GB disk + ext4, no `fcopyfile` EIO failure mode) — no observed Linux failure, just aligning both platforms' failure behavior.

### Historical context (for C1/C2 above)

**Fixed — incremental-refresh OOM crash-loop (2026-07-04):** `IndexManager::perform_incremental_refresh_with_stores` (`src/index/manager.rs`) used to chunk + embed the ENTIRE changed-file delta in one unbounded in-memory `Vec` before writing anything to the stores. A normal incremental delta was harmless; a vendor sync dropping thousands of files at once OOM'd the 1 vCPU/2 GiB `codesearch-serve` container, which then crash-looped. Fixed by batching: `changed_files.chunks(batch_size)` processed sequentially (chunk+embed+insert+commit per batch, single `build_index()` at the end), bounding peak memory to O(batch) regardless of delta size. Batch size defaults to `INCREMENTAL_REFRESH_BATCH_SIZE = 200` (`src/constants.rs`), override via `CODESEARCH_INCREMENTAL_BATCH_SIZE`. No test for the multi-batch path itself (existing `manager.rs` tests avoid real embedding, same reasoning as the gated `csharp_helper_integration` test) — verify end-to-end on a real large corpus if in doubt.

This also explains an earlier cosmetic symptom: the `docs` repo's `/status` staying on `open`/`write` for 4+ minutes after a cold start (never blocked queries — search worked within ~10-25s of the replica becoming reachable). Root cause was the same unbounded-batch warmup path, not a separate status-tracking bug.

---

## ⚠️ Branching & PR workflow (READ FIRST)

This repo uses a **`develop`-based** gitflow. The GitHub default branch is `master` (`origin/HEAD → origin/master`), but `master` is **NOT** the integration branch.

- **Integration branch = `develop`.** All feature/fix/release branches merge into `develop`.
- **ALL PRs target `develop`** — pass `--base develop` to `gh pr create`, and to `/git pr create` / `/git merge`. NEVER target `master`.
- **`master`** only receives release merges from `develop` (cut at release time).
- **Merge style = merge commits** (`--merge`), not squash. Repo history is full of `Merge pull request #N`.
- **Review requirement** is enforced by a repo ruleset (not branch protection). As repo owner, override with `gh pr merge <n> --merge --admin --delete-branch`.
- Before creating a PR, **verify the base**: `gh pr view <n> --json baseRefName`. If it says `master`, retarget: `gh pr edit <n> --base develop`.

Common mistake: a subagent runs `/git pr create` with no explicit `--base`, the tooling picks `master` (GitHub default), and the PR lands against the wrong branch. Always specify `--base develop`.

> **Note (2026-07-10):** the "merge commits, not squash" rule above is about feature/fix PRs into `develop`. Release PRs (`develop → master`) are, by contrast, squash-merged — which means master's release commits never become ancestors of develop. Over time this regresses `git merge-base(master, develop)` and can produce a false `CONFLICTING` mergeable state on a release PR even when the content is identical. If that happens, do not merge `master` into `develop` directly (history rewrite) — cut a throwaway `release/vX.Y.Z` branch off `develop`, merge `origin/master -X ours` into *that* branch, verify an empty content diff, and PR it into `master` instead.

## Notes for OpenCode / agents

- **Validation:** `cargo check` and `cargo clippy` for iteration. No `--release` builds — always dev/debug until the very end.
- **Runtime:** `C:\Users\develterf\.local\bin\` — `codesearch.exe` + `helpers/csharp/scip-csharp.exe`
- **Build:** `target/release/` — outside repo (via `CARGO_TARGET_DIR`)
- **Deploy:** `..\copy-to-common.ps1` — builds + copies both binaries to `~/.local/bin/`. A running `codesearch.exe` is file-locked on Windows; stop serve before deploying.
- **Canonical paths:** NEVER call `.canonicalize()` directly. Always use `safe_canonicalize()`.
- **LMDB rule:** No two `EnvOpenOptions::open()` on same dir in same process. All access via `get_or_open_stores()` → `Arc<SharedStores>`.
- **LMDB rule — commit, never drop, a txn whose DB handle you keep:** any `open_database` / `create_database` whose handle outlives the opening transaction MUST end that transaction with `commit()`. `drop()` aborts, and LMDB closes handles opened in an aborted transaction. Storing a DBI from a dropped `RoTxn` yields a bare `EINVAL (os error 22)` on first use, with no other symptom. This shipped in `open_readonly` from the initial commit and only surfaced once read-only became a permanent mode; see `docs/cloud-bake-docs-delta-prune-vendors/worklog.md` step 8.
- **LMDB rule — open every env with `BASE_ENV_FLAGS`** (`src/lmdb_registry.rs`). heed refuses to reopen one path with different options, so a partial rollout turns a working reopen into an intermittent failure.
- **Search errors must not become empty results:** never `unwrap_or_default()` a store error on a search path. An empty result set and a failed store must stay distinguishable by the caller — "no results" is the most misleading signal this system can emit. Render error chains with `{:#}`, never `{}`; plain `{}` prints only the outermost `.context(...)` and hides the actual fault.
  - **The rule covers EVERY MCP handler, not just `search`.** `find`, `get_chunk`, `explore`, `find_imports`, `find_dependents` and the single-store `project=` paths all report store errors too. `Err` from `get_chunk` / `get_embedding` / `FtsStore::search` may never be matched as `Ok(None)`, `.ok()`, `unwrap_or_default()` or `if let Ok(..)` without an else. This defect was fixed in one sibling handler and left in the other three times across four review rounds; it is a *class*, not a site.
  - **Never state a diagnosis you did not verify.** "not found" / "may not be indexed" is a claim about the corpus, and it is wrong when the store never answered. Pass such messages through `qualify_empty_result()`.
  - **Carry failures in a type that cannot be dropped silently.** `MultiReadOutcome` is `#[must_use]` and yields results only via `into_results(&mut warnings, what)`. Reaching for `.results` and discarding `.failures` is `unwrap_or_default()` under a new name.
  - **Do not suggest a retry against a store you know is down.** Suppress `suggested_tool` when warnings are present (`retry_hint()`).
  - **A warnings channel must terminate, on every path that writes to it.** Every `*_warnings: Vec<String>` in a handler has to end in either a `warnings` field on the response or a `qualify_empty_result()` call. A channel that is written but never read is invisible to clippy *and* to the tests — it looks fixed and behaves exactly as before. **Confirming a read site exists is not enough:** `similar_warnings` had one, inside an early-return arm, so every write after that arm was discarded. Check that the read is *reachable from the last write*, and say which response path carries it.
  - **Verify a batch mechanical edit by re-running its detector, not by intent.** An edit heuristic that hit 5 of 9 sites is indistinguishable from one that hit 9 of 9 unless the post-condition grep comes back empty — and the detector must run over the *whole file including the lines the edit added*, or it will miss defects the fix itself introduced.
  - **A caller-facing literal wrapped across lines needs a `\` continuation**, or the next line's indentation becomes part of the message. Enforced by `tests/caller_facing_literals.rs`, not by review: three commits shipped this defect through reviews that were explicitly hunting it, because the mangled text still satisfies every `contains(...)` assertion. A detector that only runs by hand gets skipped on exactly the commit that needs it.
- **Tooling:** never use the bundled `codesearch` binary to investigate this repo (it's the project under development). Use codesearch **MCP tools first** for discovery (server verified working; this repo indexed as `codesearch-git`). `grep`/`Glob`/`Read` stay correct for a specific git ref / fetched PR head (codesearch only indexes the on-disk working tree), exact literal matching, or when MCP returns nothing.
