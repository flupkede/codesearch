# AGENTS.md — codesearch (features/remote-mount-selection)

## Current state

- **Version:** see `Cargo.toml` (pre-commit hook auto-bumps patch per commit on feature branches).
- **Validation:** `cargo check` for iteration, `cargo clippy -D warnings` for lint, `cargo test --lib --bins` before a branch is considered done. No `--release` builds during the fix loop — build only at the very end.
- **Deploy:** cloud peer runs the per-vendor federation split (akeneo/vendor-a/bynder/digizuite/inriver/keyshot + custom KB), image built locally via BuildKit `docker buildx --push`, all vendors reindexed and federation validated end-to-end (`project=cloud/<vendor>`).

## Implemented Features

- **Opt-in remote mount selection** (commit `1a5b3fc`) — a peer's individual projects are no longer auto-exposed; the user explicitly `remote mount`s the ones to use. `remote_mounts` allowlist in `repos.json` is the single source of truth for routing (`resolve_remote_project`), discoverability (`list_projects`/`scope_required`), TUI display, and `@peer` group fan-out (restricted to mounted projects only, never the whole peer). CLI: `codesearch remote available|mount|unmount|mounts`.
- **Remote project mounting (1-to-1 passthrough)** (branch `features/codesearch-federation`, merged) — each project a peer exposes is addressable locally as `project=<peer>/<alias>`, same as a local project. TUI renders mounts in italic/cyan with an info panel (peer URL + live status) and disables doctor/reindex/remove (those act on a local index a mount doesn't have). `FederationClient::search_project` forwards a single-project query directly to the peer. Cloud indexer job builds one index per vendor sub-folder sequentially (avoids holding every vendor's embedding model in memory at once — see OOM fix below).
- **Federation peers** — `codesearch remote add/rm/list` (local `repos.json` peer config: `alias → url, api_key, group, into_group`) + `@peer` group references; `FederationClient` search/get_chunk fan-out with RRF.
- **Cloud indexer-job split** — heavy 4 vCPU/8 GiB build job uploads a snapshot; light 1 vCPU/2 GiB serve restores it (DOCS corpus read-only). The serve replica additionally runs a **memory-bounded incremental reindex of the small custom-kb repo** after each KB `git pull` moves `HEAD` (fire-and-forget `POST /repos/custom-kb/reindex`), so new KB articles are searchable without a redeploy; the heavy DOCS corpus stays job-only. Cloud peer live + validated. See `integrations/cloud/README.md`.
- **Remote index management (`--remote`)** — `--remote <peer>` flag on `index list/add/rm` + `index reindex` verb drives a peer's management API via `FederationClient` (`ManagementOutcome`: `Ok` / `HttpError{status,reason}` / `Unreachable`). Endpoints: `GET /status`, `POST /repos {path}`, `DELETE /repos/:alias`, `POST /repos/:alias/reindex[?force=]`. `--json` on List/Reindex (requires `--remote`). Without `--remote`, every `index` verb is unchanged (local).
- **Local `index rm <alias>`** — resolves the argument as a registered alias before falling back to path interpretation.
- **CLI aliases** — `ls` is a visible alias for `list` (`index`/`groups`/`remote`); `rm` for `remove` (pre-existing).

> ℹ️ **Remote write verbs** (`add`, `reindex --force`) require a read-write peer; the cloud peer rejects them (`--force` → HTTP 500 "could only be opened read-only; cannot force-reindex"). An **incremental** `reindex` (no `--force`) of an already-registered repo *does* succeed on the cloud peer — that is the custom-kb auto-refresh path. `list` is always safe. `rm` is not durable — the next cold start re-registers from the restored snapshot.

## Deferred / follow-ups (non-blocking)

Left over from the remote-project-mounting work; not yet done:
- **Persist remote-project discovery** to a `remote_project_cache` in `repos.json` — the TUI's peer-`/status` discovery is currently in-memory-only (last-known-good survives a blip, not a process restart).
- **Extract a shared `build_remote_search_body(request, mode)`** in `src/mcp/mod.rs` — the group fan-out and single-project fan-out request bodies are still two identical 11-field blocks; drift risk if one is edited without the other.
- **Remove the now-dead `wait_until_indexed()`** in `docker/entrypoint.sh` — superseded by the sequential `wait_active_build_done()` loop, never called anymore.

## Fixed — incremental-refresh OOM crash-loop (2026-07-04)

`IndexManager::perform_incremental_refresh_with_stores` (`src/index/manager.rs`) used to chunk + embed the ENTIRE changed-file delta in one unbounded in-memory `Vec` before writing anything to the stores. A normal incremental delta (tens of files) was harmless; a vendor sync dropping thousands of files at once OOM'd the 1 vCPU/2 GiB `codesearch-serve` container, which then crash-looped re-running the full azcopy sync every restart (`/status`/`/search` unreachable for minutes). Fixed by batching: `changed_files.chunks(batch_size)` processed sequentially (chunk+embed+insert+commit per batch, single `build_index()` at the end), bounding peak memory to O(batch) regardless of delta size. Batch size defaults to `INCREMENTAL_REFRESH_BATCH_SIZE = 200` (`src/constants.rs`), override via `CODESEARCH_INCREMENTAL_BATCH_SIZE`. Protects both `codesearch-serve`'s in-process warmup and `codesearch-indexer`'s full rebuild. No test for the multi-batch path itself (existing `manager.rs` tests avoid real embedding, same reasoning as the gated `csharp_helper_integration` test) — verify end-to-end on a real large corpus if in doubt.

This also explains an earlier cosmetic-looking symptom: the `docs` repo's `/status` staying on `open`/`write` for 4+ minutes after a cold start (never blocked queries — search worked within ~10-25s of the replica becoming reachable). Root cause was the same unbounded-batch warmup path, not a separate status-tracking bug.

## Still open — automating the "manual scaling" question

Confirmed (2026-07-04): `codesearch-indexer` job has `triggerType: "Manual"` — nothing runs it automatically today; every rebuild has been a human running `az containerapp job start` by hand. The batching fix above means a large batch can no longer crash anything, but staleness is still only resolved manually. Options discussed, not yet decided (needs vendor content update-cadence info the agent doesn't have):
- **Schedule trigger** on the existing job (`az containerapp job update --trigger-type Schedule --cron-expression "..."`) — no new Azure resources, just a cron cadence. Cost/staleness tradeoff depends on how often the vendor export actually changes upstream.
- **Event-driven** (Event Grid on the blob source triggering job start) — more precise, needs a new Event Grid subscription + small trigger function/Logic App.
- The single-app scale-up/poll/snapshot/scale-down redesign for `codesearch-serve` itself (below) remains a separate, bigger follow-up.

## Proposed redesign — collapse indexer job + serve into one scalable app

**Problem with the current split:** `codesearch-indexer` (4 vCPU/8GiB, full/incremental build + snapshot upload) and `codesearch-serve` (1 vCPU/2GiB, restore-only) only talk to each other via a blob-storage snapshot round-trip. Every content update pays for a full tar-upload + download-untar cycle, and `serve`'s own incremental-warmup step duplicates part of the indexer's job on hardware sized for read-only serving — which is what caused the crash-loop above.

**Why the round-trip exists at all:** the index store is **LMDB** (mmap-based), which is not safe on network-mounted volumes (Azure Files/NFS — mmap needs local POSIX byte-range locking a network share can't reliably provide). So the index must live on local ephemeral disk, and ephemeral disk does not survive a Container Apps revision change (any `--cpu`/`--memory` update triggers one) — hence some durable handoff (blob snapshot) is unavoidable across a resource-tier change.

**Proposed design (single app, no separate job):**
1. `az containerapp update -n codesearch-serve --cpu 2.0 --memory 4Gi` — new revision, cold start (restore last snapshot, sync corpus, start incremental reindex in-process).
2. Poll `GET /status` every ~10-15s (generous timeout, e.g. 15 min) until **all repos report `"status": "warm"`** — replaces the fragile in-process `indexing`-flag/120s-timeout detection in `entrypoint.sh` that caused the crash above.
3. Once warm, trigger a snapshot upload (existing `upload_snapshot` logic).
4. `az containerapp update -n codesearch-serve --cpu 1.0 --memory 2Gi` — new revision, cold start, restore-only from the snapshot just uploaded.

**What this fixes:** one Container App resource instead of two; a robust, externally-observable completion signal instead of a flaky internal flag; the blob round-trip still happens (structurally required — see above) but only once per deliberate scale-cycle instead of as a side effect of a separate job existing.

**Not yet decided:** whether to retire `codesearch-indexer` entirely or keep it only for disaster-recovery-style full rebuilds, and whether the scale-up/poll/snapshot/scale-down cycle should be a scheduled script, a Logic App, or a wrapper CLI command (`codesearch cloud rebuild --remote <peer>`?).

**Scoped first step shipped (2026-07-08):** the "incremental reindex in-process on serve" idea is live — but *only* for the small **custom-kb** repo (`docker/entrypoint.sh`'s serve-mode KB pull loop fires `POST /repos/custom-kb/reindex` whenever `git pull` moves `HEAD`). Safe on the 1-2 GiB replica because incremental refresh is memory-bounded (see fix above) and the KB corpus is tiny. The heavy DOCS corpus deliberately stays job-only — re-embedding thousands of files in-process is exactly the OOM that motivated the split. The full self-scaling redesign for the DOCS corpus (above) remains a separate, undecided follow-up.

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
- **Tooling:** do not use the bundled `codesearch` binary to investigate this repo (it's the project under development). Use codesearch MCP tools when available, else `grep`/`Glob`/`Read`.
