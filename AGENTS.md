# AGENTS.md — codesearch (features/remote-mount-selection)

## Current Plan — opt-in remote mount selection (2026-07-07) ✅ CODE COMPLETE

**Refines the project-mounting work below.** The earlier design auto-discovered and mounted
*every* project a peer exposed (opt-out via `remote_hidden`). Per user intent, selection is now
**opt-in**: after `remote add`, the local user explicitly chooses which individual per-vendor
indexes to use.

**Locked decisions (2026-07-07):**
- **`remote_mounts` allowlist = single source of truth** (canonical `<peer>/<alias>` in
  `repos.json`). Replaces the opt-out `remote_hidden`; nothing auto-mounts.
- **Group fan-out restricted to mounts:** an `@peer` reference in a group federates only that
  peer's *mounted* indexes (each as its own `project=` query), never the whole peer.
- **Non-mounted = unroutable:** `resolve_remote_project` gates on the allowlist.

**Done (commit `1a5b3fc`):**
- `repos.rs`: `remote_mounts`; `mounted_remote_projects()` allowlist-driven (no discovery arg);
  `resolve_remote_project()` allowlist gate; `group_remote_projects()`; `mount_remote_project()`/
  `unmount_remote_project()`; `reconcile()` prunes stale/unknown-peer/malformed mounts + orphan
  rename overrides.
- `mcp/mod.rs`: `federated_search` fans out per mounted project (`search_project`); obsolete
  whole-peer `FederationClient::search` removed; `list_projects` gains a `remote_projects` array;
  `scope_required` advertises mounted names in `available_projects`.
- `cli/mod.rs`: `remote available|mount|unmount|mounts`.
- `serve/tui.rs`: rows come from the allowlist; peer discovery only enriches live status.
- Docs: CHANGELOG / README / AGENTS updated.

## Current Plan — remote project mounting (1-to-1 passthrough)

**Goal.** Move federation from *group-level* (`docs = [@cloud]`, all remote repos hidden
behind one reference) to *project-level mounting*: each index a peer exposes appears locally
as a first-class project, routable with `project=<peer>/<alias>` **as if it were local**, and
shown *italic* in the local TUI to signal it lives on a peer. The server-imposed `docs` bundle
is dropped; grouping becomes a purely-local, user-owned composition (a local `docs` group with
several remote members stays possible — the user decides).

**Locked design decisions (2026-07-06):**
- **Discovery = auto-discover + local filter.** *(SUPERSEDED by the opt-in plan above —
  selection is now an explicit `remote_mounts` allowlist, not auto-discover-everything.)* On
  startup the local instance queries each peer's `GET /status`, enumerates its repos, and mounts
  them as remote projects. The user can hide/rename specific mounts locally. Peer unreachable at
  startup → fall back to last-known cached list (never hard-fail).
- **Naming = peer-namespaced.** Remote projects are named `<peer>/<alias>` (e.g. `cloud/vendor-a`)
  — always unambiguous, never shadows a local repo, TUI shows the source at a glance.

**Why (beyond ranking):** smaller per-vendor indexes → smaller/faster rebuilds, per-vendor
incremental reindex, lower peak memory (synergy with the incremental-refresh batching fix).
Tradeoff: N snapshots/blobs instead of 1 → more azcopy/sync overhead. Fair cross-repo RRF (small
vendors no longer drowned by large ones) + `project=<vendor>` routing with zero cross-vendor
competition.

**Current code gaps (verified):**
- `ReposConfig::resolve(project)` is **local-only** (`self.repos.get`) — never yields a
  `Target::Remote`. Only `resolve_group_targets` federates. This is the core gap.
- MCP `project=` dispatch (`src/mcp/mod.rs` ~3989) routes single projects locally only.
- `FederationClient` fans out *group* queries; needs a single-remote-project query path
  (the peer's `/search` already accepts `project=`).
- TUI `RepoRow` / `tui_common::render_table` has no `is_remote`/italic styling; the local
  dashboard doesn't include mounted remote projects. (`tui_remote.rs` is a *separate* standalone
  remote dashboard, not the inline-mount view.)

**Staged execution — ALL STAGES COMPLETE (branch `features/codesearch-federation`):**
- **Stage 1 ✅** — Config model: mounted remote projects + auto-discovery + local hide/rename
  filter in `repos.rs` (`RemotePeer` discovery, `<peer>/<alias>` namespace, cache fallback).
- **Stage 2 ✅** — Single-project remote resolution + MCP `project=<peer>/<alias>` dispatch
  routing (local-first precedence: a local alias always wins a name clash).
- **Stage 3 ✅** — `FederationClient::search_project` single-remote-project query (forwards
  `project=<alias>` to the peer, strips `group`; shared `post_search` helper). Merged with
  Stage 2 as one "routing" commit since dispatch can't compile without the client method.
- **Stage 4 ✅** — TUI: `is_remote` on `RepoRow`, italic (cyan) rendering in table + detail,
  background peer-`/status` discovery (30s cadence, capacity-1 channel, in-memory last-known
  fallback) appending mounted remote projects to the local dashboard. Mount rows also support
  `i` (info → `OverlayState::RemoteInfo` panel: peer URL + peer-reported status) and render the
  footer's `doctor`/`reindex`/`remove` hints struck-through/disabled, since those act on a
  local index a mount doesn't have.
- **Stage 5 ✅** — Indexer-job split (`docker/entrypoint.sh`): builds one index per
  `/data/docs/<vendor>` subfolder (loop + fail-fast on none; verify-every-vendor before
  upload) instead of a single monolithic `docs` repo. Coupled azcopy `--exclude-path` fix
  (`docs_index_exclusions`) protects each `<vendor>/.codesearch.db` from `--delete-destination`
  on both job and serve cold-start restore. Vendors are built **sequentially** (each build is
  awaited to "settle" before the next starts): a parallel-submit variant OOM-killed the 8 GiB
  serve replica by making it hold every vendor's embedding model + working set at once.

**Deferred (post-merge / future cleanup, non-blocking):**
- Persist discovery to `remote_project_cache` in `repos.json` for cross-restart fallback
  (Stage 4 uses in-memory last-known only — sufficient for blips, not process restarts).
- Extract a shared `build_remote_search_body(request, mode)` in `src/mcp/mod.rs` (the group
  and single-project fan-out bodies are identical 11-field blocks — drift risk only).
- Persist remote-project discovery across serve restarts (see cache note above).
- Clean up the now-unused `wait_until_indexed()` dead code in `docker/entrypoint.sh` (superseded
  by the sequential `wait_active_build_done()` loop).

## Current state

- **Branch:** `features/remote-mount-selection` (branched from `develop` after the federation merge)
- **Version:** v1.1.11 (opt-in remote mount selection; pre-commit hook auto-bumps patch per commit)
- **Deploy:** cloud peer redeployed with the per-vendor federation split (akeneo/vendor-a/bynder/digizuite/inriver/keyshot + custom KB), image built locally via BuildKit `docker buildx --push`, all vendors reindexed and federation validated end-to-end (`project=cloud/<vendor>`).
- **Status:** `cargo check` + `cargo clippy` clean
- **Validation:** `cargo check` for iteration, `cargo clippy` for lint. No `--release` builds during the fix loop; build only at the very end.

## Implemented on this branch

- **Federation peers** — `codesearch remote add/rm/list` (local `repos.json` peer config: `alias → url, api_key, group, into_group`) + `@peer` group references; `FederationClient` search/get_chunk fan-out with RRF.
- **Cloud indexer-job split** — heavy 4 vCPU/8 GiB build job uploads a snapshot; light 1 vCPU/2 GiB serve restores it (DOCS corpus read-only). The serve replica additionally runs a **memory-bounded incremental reindex of the small custom-kb repo** after each KB `git pull` moves `HEAD` (fire-and-forget `POST /repos/custom-kb/reindex`), so new KB articles are searchable without a redeploy; the heavy DOCS corpus stays job-only. Snapshot refresh/verify loop. Cloud peer live + validated. See `integrations/cloud/README.md`.
- **Remote index management (`--remote`)** — `--remote <peer>` flag on `index list/add/rm` + new `index reindex` verb drives a peer's management API via `FederationClient` (`ManagementOutcome`: `Ok` / `HttpError{status,reason}` / `Unreachable`). Endpoints: `GET /status`, `POST /repos {path}`, `DELETE /repos/:alias`, `POST /repos/:alias/reindex[?force=]`. `--json` on List/Reindex (requires `--remote`). Without `--remote`, every `index` verb is unchanged (local).
- **Local `index rm <alias>`** — resolves the argument as a registered alias before falling back to path interpretation.
- **CLI aliases** — `ls` is a visible alias for `list` (`index`/`groups`/`remote`); `rm` for `remove` (pre-existing).

> ℹ️ **Remote write verbs** (`add`, `reindex --force`) require a read-write peer; the cloud peer rejects them (`--force` → HTTP 500 "could only be opened read-only; cannot force-reindex"). An **incremental** `reindex` (no `--force`) of an already-registered repo *does* succeed on the cloud peer — that is the custom-kb auto-refresh path. `list` is always safe. `rm` is not durable — the next cold start re-registers from the restored snapshot. Per-vendor sub-path registration is scripted against a writable peer.

## Known issue — `docs` repo status stuck on `open`/`write` after cold start (cloud)

**Repro (2026-07-01):** on the cloud `codesearch-serve` (restore-only mode), forced two cold
restarts via `az containerapp revision restart`. After each restart:
- `repo-a` repo (custom KB, smaller corpus) flips `open` → `warm` quickly, as expected.
- `docs` repo (6 harvested vendor sources, 9977 chunks / 2509 files) **stayed on
  `status: "open"`, `lock_mode: "write"`** for 4+ minutes straight (polled every 5-7s) and
  never flipped to `warm` in the observation window.

**But this does NOT block queries** — `/search` against `project=docs` returned correct
results with ~280-300ms latency starting within ~1s of the new replica becoming reachable,
the entire time `status` claimed `open`/`write`. Cold-start-to-working-search was measured at
**~10-25s total** (restart trigger → first real search result), which is fine; the confusing
part is purely the status field, not actual availability.

**Hypothesis:** a stuck/orphaned warmup or lock flag specific to multi-file corpora on the
restore-only path — possibly the incremental-warmup routine that's supposed to flip the repo
from `open`→`warm` post-snapshot-restore never completes/clears for `docs`, while `repo-a`
(fewer files) finishes fast enough that the flag clears normally. Needs investigation:
- Check `evict_idle_repos` / warmup-completion logic in `src/serve/mod.rs` for a path that
  can leave `status` and `lock_mode` desynced from actual query-readiness.
- Confirm whether `docs`'s size (2509 files) crosses some batch/chunking threshold that
  `repo-a` doesn't.
- Add a regression check: after cold start, poll `/repos/<alias>/info` + `/status` until
  `warm`, with a timeout — if it never flips, that itself is the bug reproduction.

**Priority: escalated to HIGH (2026-07-04).** Originally filed as cosmetic/status-only. Now
confirmed as the same underlying mechanism behind a real crash-loop: after the vendor `docs`
corpus roughly doubled (2509 -> 5666 files), `codesearch-serve` (1 vCPU/2GiB) entered a crash
loop on cold start — the "serve startup warmup is incrementally refreshing it" step tries to
re-embed the delta in-process, took >120s (past the entrypoint's own wait-for-`indexing`-flag
window, logged as `WARN: no 'indexing' observed within 120s — proceeding cautiously`), and the
container OOM'd/restarted repeatedly, re-running the full azcopy sync every time. `/status` and
`/search` were unreachable (timeouts / 503) for several minutes until a manual
`codesearch-indexer` job run produced a fresh snapshot. Root cause and fix below supersede the
narrower serve/mod.rs theory.

> ⚠️ **No Azure/PIM access needed to investigate the code path.** `src/serve/mod.rs` and
> `docker/entrypoint.sh` warmup/lock logic can be reasoned about from source. Reproducing the
> crash locally needs a large-enough local repo (e.g. `repo-large`, 25751 chunks / 2831 files)
> restarted via local `codesearch serve`. Only touch the cloud (and thus PIM) to verify a fix
> against the real corpus size.

**Actual root cause found + fixed (2026-07-04):** `IndexManager::perform_incremental_refresh_with_stores`
(`src/index/manager.rs`) chunked + embedded the ENTIRE changed-file delta in one unbounded
in-memory `Vec` before writing anything to the stores. A normal incremental delta (tens of
files) is harmless; a vendor sync dropping thousands of files at once is not — that unbounded
batch is what OOM'd the 1 vCPU/2 GiB `codesearch-serve` container. Fixed by batching: the loop
now processes `changed_files.chunks(batch_size)` sequentially (chunk+embed+insert+commit per
batch, single `build_index()` at the end), bounding peak memory to O(batch) regardless of
delta size. Batch size defaults to `INCREMENTAL_REFRESH_BATCH_SIZE = 200`
(`src/constants.rs`), override via `CODESEARCH_INCREMENTAL_BATCH_SIZE`. `cargo check` +
`cargo clippy -D warnings` + `cargo test --lib --bins` all clean. This fix is independent of
which container runs it — it protects `codesearch-serve`'s in-process warmup **and**
`codesearch-indexer`'s full rebuild against the same failure mode as the corpus keeps growing.
No test added for the multi-batch path itself: existing `manager.rs` tests deliberately avoid
invoking real embedding (slow/ONNX-model-dependent, same reasoning as the gated
`csharp_helper_integration` test) — verify end-to-end on a real large corpus if in doubt.

## Still open — automating the "manual scaling" question

Confirmed (2026-07-04): `codesearch-indexer` job has `triggerType: "Manual"` — nothing runs it
automatically today; every rebuild has been a human running `az containerapp job start` by
hand. The code fix above means a large batch can no longer crash anything, but staleness is
still only resolved manually. Options discussed, not yet decided (needs vendor content
update-cadence info the agent doesn't have):
- **Schedule trigger** on the existing job (`az containerapp job update --trigger-type Schedule
  --cron-expression "..."`) — no new Azure resources, just a cron cadence. Cost/staleness
  tradeoff depends on how often the vendor ServiceNow export actually changes upstream.
- **Event-driven** (Event Grid on the blob source triggering job start) — more precise, needs
  a new Event Grid subscription + small trigger function/Logic App.
- The previously-proposed single-app scale-up/poll/snapshot/scale-down redesign for
  `codesearch-serve` itself (below) remains a separate, bigger follow-up.

## Proposed redesign — collapse indexer job + serve into one scalable app

**Problem with the current split:** `codesearch-indexer` (4 vCPU/8GiB, full/incremental build
+ snapshot upload) and `codesearch-serve` (1 vCPU/2GiB, restore-only) are two separate Container
Apps resources that only talk to each other via a blob-storage snapshot round-trip. Every
content update pays for a full tar-upload + download-untar cycle, and `serve`'s own "helpful"
incremental-warmup step duplicates part of the indexer's job on hardware sized for read-only
serving — which is what caused the crash-loop above.

**Why the round-trip exists at all:** the index store is **LMDB** (mmap-based). LMDB is not
safe on network-mounted volumes (Azure Files/NFS) — mmap needs local POSIX byte-range locking
guarantees a network share can't reliably provide, risking corruption. So the index must live
on local ephemeral disk, and ephemeral disk does **not** survive a Container Apps revision
change (which is what any `--cpu`/`--memory` update triggers) — hence *some* durable handoff
(blob snapshot) is unavoidable across a resource-tier change.

**Proposed design (single app, no separate job):**
1. `az containerapp update -n codesearch-serve --cpu 2.0 --memory 4Gi` — new revision, cold
   start (restore last snapshot, sync corpus, start incremental reindex in-process).
2. Poll `GET /status` every ~10-15s with a generous timeout (e.g. 15 min) until **all repos
   report `"status": "warm"`** — replaces the fragile in-process `indexing`-flag/120s-timeout
   detection in `entrypoint.sh` that's the proximate cause of the crash above.
3. Once warm, trigger a snapshot upload (existing `upload_snapshot` logic).
4. `az containerapp update -n codesearch-serve --cpu 1.0 --memory 2Gi` — new revision, cold
   start, restore-only from the snapshot just uploaded (small/fast since it's current).

**What this fixes:** one Container App resource instead of two; a robust, externally-observable
completion signal instead of a flaky internal flag; the blob round-trip still happens (ACA
ephemeral disk can't survive a resource-tier change, so a durable handoff is structurally
required) but now happens exactly once per deliberate scale-cycle instead of as an accidental
side effect of a separate job existing.

**Not yet decided:** whether to retire `codesearch-indexer` entirely or keep it only for
disaster-recovery-style full rebuilds. Whether the scale-up/poll/snapshot/scale-down cycle
should be a scheduled script, a Logic App, or a small wrapper CLI command
(`codesearch cloud rebuild --remote <peer>`?) is open for the next session.

**Scoped first step shipped (2026-07-08):** the "incremental reindex in-process on serve" idea
is now live — but *only* for the small **custom-kb** repo. `docker/entrypoint.sh`'s serve-mode
KB pull loop fires an incremental `POST /repos/custom-kb/reindex` whenever a `git pull` moves
`HEAD`. This is safe on the 1–2 GiB replica because (a) incremental refresh is memory-bounded
(`INCREMENTAL_REFRESH_BATCH_SIZE`, see the crash-loop fix above) and (b) the KB corpus is tiny.
The heavy DOCS corpus deliberately stays job-only — re-embedding thousands of files in-process
is exactly the OOM that motivated the split. The full single-app self-scaling redesign for the
DOCS corpus (above) remains a separate, undecided follow-up.

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

## Notes for OpenCode / agents

- **Validation:** `cargo check` and `cargo clippy` for iteration. No `--release` builds — always dev/debug until the very end.
- **Runtime:** `C:\Users\develterf\.local\bin\` — `codesearch.exe` + `helpers/csharp/scip-csharp.exe`
- **Build:** `target/release/` — outside repo (via `CARGO_TARGET_DIR`)
- **Deploy:** `..\copy-to-common.ps1` — builds + copies both binaries to `~/.local/bin/`. A running `codesearch.exe` is file-locked on Windows; stop serve before deploying.
- **Canonical paths:** NEVER call `.canonicalize()` directly. Always use `safe_canonicalize()`.
- **LMDB rule:** No two `EnvOpenOptions::open()` on same dir in same process. All access via `get_or_open_stores()` → `Arc<SharedStores>`.
- **Tooling:** do not use the bundled `codesearch` binary to investigate this repo (it's the project under development). Use codesearch MCP tools when available, else `grep`/`Glob`/`Read`.
