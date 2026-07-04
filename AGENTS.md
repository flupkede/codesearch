# AGENTS.md — codesearch (features/codesearch-federation)

## Current state

- **Branch:** `features/codesearch-federation`
- **Version:** v1.1.0 (federation GA)
- **Status:** `cargo check` + `cargo clippy` clean
- **Validation:** `cargo check` for iteration, `cargo clippy` for lint. No `--release` builds during the fix loop; build only at the very end.

## Implemented on this branch

- **Federation peers** — `codesearch remote add/rm/list` (local `repos.json` peer config: `alias → url, api_key, group, into_group`) + `@peer` group references; `FederationClient` search/get_chunk fan-out with RRF.
- **Cloud indexer-job split** — heavy 4 vCPU/8 GiB build job uploads a snapshot; light 1 vCPU/2 GiB serve restores it read-only; snapshot refresh/verify loop. Cloud peer live + validated. See `integrations/cloud/README.md`.
- **Remote index management (`--remote`)** — `--remote <peer>` flag on `index list/add/rm` + new `index reindex` verb drives a peer's management API via `FederationClient` (`ManagementOutcome`: `Ok` / `HttpError{status,reason}` / `Unreachable`). Endpoints: `GET /status`, `POST /repos {path}`, `DELETE /repos/:alias`, `POST /repos/:alias/reindex[?force=]`. `--json` on List/Reindex (requires `--remote`). Without `--remote`, every `index` verb is unchanged (local).
- **Local `index rm <alias>`** — resolves the argument as a registered alias before falling back to path interpretation.
- **CLI aliases** — `ls` is a visible alias for `list` (`index`/`groups`/`remote`); `rm` for `remove` (pre-existing).

> ℹ️ **Remote write verbs** (`add`, `reindex --force`) require a read-write peer; the restore-only cloud peer rejects them (`--force` → HTTP 500 "could only be opened read-only; cannot force-reindex"). `list` is always safe. `rm` is not durable — the next cold start re-registers from the restored snapshot. Per-vendor sub-path registration is scripted against a writable peer.

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
