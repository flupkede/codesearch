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

**Priority:** low (cosmetic/status-only, not a functional blocker) but worth fixing since it
undermines trust in the `/status` health signal for monitoring/alerting.

> ⚠️ **No Azure/PIM access needed to investigate this.** The fix is pure code analysis
> (`src/serve/mod.rs` warmup/lock logic) and can be reproduced **locally** first — this repo
> already has large multi-file local repos registered (e.g. `repo-large`, 25751 chunks /
> 2831 files) that can be cold-restarted via local `codesearch serve` to check whether the
> same `open`/`write`-stuck behavior reproduces without touching the cloud at all. Only reach
> for the cloud (and thus PIM) if the bug turns out to be specific to the restore-only /
> snapshot-restore cold-start path and doesn't reproduce locally.

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
