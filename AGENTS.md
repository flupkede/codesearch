# AGENTS.develop.md

This file is the **develop-branch reference** for all coding agents.
It is committed on `develop` and merged from every feature branch.

`AGENTS.md` and `CLAUDE.md` are **branch-local working files**: committed on the
feature branch while building, dropped before the PR merges — they never appear
on `develop` or `master`. See *For agents starting a new feature branch* below.

## Setup (once per machine)

```bash
git config core.hooksPath .githooks
```

This enables the hooks: `post-checkout` auto-creates `AGENTS.md` (from this
file) and the `CLAUDE.md` pointer on feature branches, `pre-commit` runs
`cargo fmt` + the root-md allowlist guard (branch-aware, see below), `pre-push`
blocks direct pushes to `master`. See `.githooks/README.md`.

## For agents starting a new feature branch

If `AGENTS.md` was not auto-created by the hook, create it manually:

```bash
cp AGENTS.develop.md AGENTS.md
```

Then append your work plan as a `## Plan` section at the end of `AGENTS.md`.
Leave the architecture sections intact — they provide context.

**Commit `AGENTS.md`/`CLAUDE.md` on the feature branch as you work** — the plan
travels through the PR and survives branch switches.

**At the end of the branch, before the PR (the changelog moment):**
1. Update `## Active feature branches` below (remove this branch)
2. Add one line to `## Changelog highlights`
3. Add the CHANGELOG.md entry under the pending-version heading (functionality changes only — pure tooling/docs churn gets the `no-changelog` PR label instead; workflow changes like this one never go in the changelog)
4. Drop the branch-local files and commit:
   `git rm AGENTS.md CLAUDE.md` → `chore: drop branch-local agent files`

Enforced mechanically (forgetting step 4 cannot leak the files):
- `pre-commit` rejects introducing root `AGENTS.md`/`CLAUDE.md` on
  `develop`/`master` (direct commits and conflict resolutions).
- `pre-merge-commit` runs the same guard on the merge result — a clean
  merge never invokes pre-commit.
- The `agent-files-check` CI workflow flags any PR into `develop`/`master`
  whose head still carries them.

**No active work plan lives here.** Feature branches carry their own `AGENTS.md`.
This file contains only architecture, conventions, and changelog.

---

## ⚠️ Branching & PR workflow (READ FIRST)

This repo uses a **`develop`-based** gitflow. The GitHub default branch is `master` (`origin/HEAD → origin/master`), but `master` is **NOT** the integration branch.

- **Integration branch = `develop`.** All feature/fix/release branches merge into `develop`.
- **ALL PRs target `develop`** — pass `--base develop` to `gh pr create`. NEVER target `master` (releases only, cut at release time).
- **Merge style:** feature/fix → `develop` = merge commits (`--merge`); `develop` → `master` release PR = **squash**, with `--body "$(scripts/release-coauthors.sh)"` so contributors stay credited on the default branch.
- **Review requirement** is enforced by a repo ruleset. As repo owner, override with `gh pr merge <n> --merge --admin --delete-branch`.
- Before creating a PR, **verify the base**: `gh pr view <n> --json baseRefName`. If it says `master`, retarget: `gh pr edit <n> --base develop`.
- **Release squashes regress the merge-base** → a release PR may fail with "cannot be cleanly created" even when content is fine. Fix: cut `release/vX.Y.Z` off develop, run `git merge -s ours origin/master` in it, verify the diff vs master is the intended release delta, PR that into master. Never merge master into develop directly.

Common mistake: a subagent creates a PR with no explicit `--base`, the tooling picks `master` (GitHub default), and the PR lands against the wrong branch. Always specify `--base develop`.

---

## What codesearch is

A fast, local, offline MCP server for semantic code search. Single Rust binary.
No Docker, no cloud, no external services. Designed for coding agents (OpenCode,
Claude Code, Claude Desktop) that need to search and navigate large codebases efficiently.

Core stack: Tantivy (BM25 FTS) + arroy (HNSW vectors) + fastembed/ONNX (embeddings) +
tree-sitter (AST chunking) + LMDB (persistent storage) + rmcp 1.5.0 (MCP protocol).
Hybrid BM25 + vector search fused via RRF.

## MCP tools

Five tools exposed to agents:

| Tool | Description |
|---|---|
| `search` | Hybrid semantic + BM25 search. Requires `project` or `group` in multi-repo mode. |
| `find` | Symbol navigation: definition, usages, imports, dependents. Requires scope. |
| `explore` | File outline or similar-chunk lookup. Requires scope. |
| `get_chunk` | Retrieve a chunk by ID with optional context lines. Requires `project` in multi-repo mode. |
| `status` | Index and project status. Lightweight (no DB open) when called without scope. |

All tools return `scope_required` structured errors in multi-repo mode when no `project`
or `group` is specified, with `available_projects`, `available_groups`, and `hint_for_agent`.

## Multi-repo serve mode

`codesearch serve` starts an MCP HTTP server on `127.0.0.1:{port}` (default 39725).
Multiple repos register via `repos.json` with aliases. Agents route queries per-alias
or per-group.

**Repo lifecycle:**
- `Warm` — DB open, vector index ready, no FSW. State after background warmup or fan-out open.
- `Write` — Warm + file system watcher running. Transitions from Warm on first explicit project query.
- `Readonly` — Another process holds the write lock.
- `Closed` — Evicted by idle reaper after `REPO_IDLE_TIMEOUT_SECS` (30 min default) of inactivity.

**Idle reaper** runs every `REAPER_INTERVAL_SECS` (5 min). Evicts repos not queried within timeout.
All opens update `last_access` so the reaper can track every open.

**Fan-out rule:** `get_chunk` and group queries use `touch=false` → repos open as Warm only,
no FSW spawned. Only explicit `project=` queries use `touch=true` → Warm→Write transition.

## TUI

`codesearch serve` with a TTY starts an embedded ratatui TUI (repo table, status, CPU).
Without TTY: headless, logs only.

- `codesearch serve --no-tui` — suppress TUI even with TTY
- `codesearch serve tui [--url http://...]` — standalone TUI via HTTP polling of `GET /status`

## HTTP endpoints (serve mode)

| Endpoint | Method | Description |
|---|---|---|
| `/health` | GET | Health check JSON |
| `/status` | GET | Lightweight repo state snapshot for TUI polling |
| `/repos` | POST | Register + index + warmup a new repo |
| `/repos/:alias` | DELETE | Stop FSW, evict, unregister, delete DB |
| `/repos/:alias/reindex` | POST | Incremental or force reindex (background) |
| `/mcp` | GET/POST | MCP streamable HTTP endpoint |

## Supported languages

17 tree-sitter grammars (table in README.md). `find_impact` has SCIP symbol
precision for C# (bundled `scip-csharp`, resident helper via its `serve`
subcommand) and TypeScript (`npx scip-typescript`). Protobuf is text-aware
chunking only.

## Release artifacts

GitHub Actions produces release binaries per tag (plain + `-with-csharp`
variants): `codesearch-{windows-x86_64.zip,linux-x86_64.tar.gz,macos-arm64.tar.gz}`.
Single-file native binaries, no runtime dependencies. macOS build is manual-trigger
only (expensive runners).

---

## Current state

- **Version:** `Major.Minor.Patch` (semver). Patch auto-bumps +1 on every PR merged to `develop` (`.github/workflows/bump-develop.yml`); minor bumps manually at release (`scripts/bump-version.sh --type minor`, resets patch→0). Per-commit uniqueness: `build.rs` `+<commit_count>`. See `RELEASING.md`.
- **Validation:** `cargo check` for iteration → `cargo clippy --all-targets -- -D warnings` → `cargo test --lib --bins` before a branch is done. No `--release` builds during the fix loop.
- **Deploy:** `..\copy-to-common.ps1` builds + copies both binaries to `~/.local/bin/`. Stop serve first — a running `codesearch.exe` is file-locked on Windows.

## Implemented features (orientation only — narratives live in CHANGELOG.md)

- **Federation** — `codesearch remote add/rm/list/mounts`; `@peer` group fan-out; mounted projects addressable as `project=<peer>/<alias>`; TUI mount management with live status. Remote write verbs (`add`, `reindex --force`) need a read-write peer; `rm` is not durable across a cold start.
- **Cloud indexer-job split** — heavy build job uploads a snapshot, light serve restores it. DOCS repos are `repo_read_only` in `repos.json`; only `custom-kb` stays writable with a bounded incremental reindex per KB `git pull`.
- **`find_impact`** — SCIP-precise transitive call-sites (C#, TypeScript), ambiguity envelope, partial-results warnings, freshness fields. On `busy: true` sleep `retry_after_seconds` and retry the SAME call (busy is progress, never a reason to fall back to text search); `index_head_sha` vs `current_head_sha` drift is surfaced, never auto-reindexed.
- **REST mirrors** — read-only HTTP mirrors of the MCP tools incl. `/find-impact`.

## ⚠️ Design constraint — never poll a federated peer

**A federated peer is NEVER contacted on a timer.** Each poll wakes the peer's
scale-to-zero replica, which then self-warms its full idle window (measured ~50%
duty cycle from zero searches). Shipped twice (PR #181/#184), reverted twice —
do not re-attempt. Local repos may be polled in the background; peers are
contacted only by a real federated tool call (activity poke) or the explicit
TUI `i` overlay. The TUI discovery tick is config-only (zero HTTP).

## Open TODOs

- **Low severity:** literal-mode snippets for markdown chunks sometimes show the chunk's opening line instead of the matched line (the `match_info.unwrap_or_else` fallback), making true hits look like false negatives.

---

## Key conventions for agents

- **Branch from develop**, never from master. Feature branches: `features/<name>` (or `fix/`, `chore/`).
- **Cargo.toml version** on develop may be one version ahead of the deployed binary — that is expected due to `copy-to-common.ps1` deploy hook. Never flag as inconsistency.
- **Always fix bugs you encounter — including pre-existing ones** the current branch did not introduce. Clean code and tests are the bar: a fix lands with a test that fails without it (reintroduce the defect, watch it fail — the proof rule under *Search errors*).
- **Never write separate `AGENTS_xxx.md` sibling files** unless explicitly requested. OpenCode reads `AGENTS.md` only. Out-of-repo planning goes to `C:\WorkArea\AI\codesearch\instructions\`.
- **Root file hygiene (markdown)**: root markdown is allowlisted — `AGENTS.develop.md`, `README.md`, `README_CSharp.md`, `CHANGELOG.md`, `RELEASING.md` always; `AGENTS.md` and `CLAUDE.md` on **feature branches only** (branch-local, dropped at finalization — the pre-commit guard rejects them on `develop`/`master`). Anything else (diagnoses, plans, test scenarios, worklogs) goes into `.docs/` (gitignored, local-only). Enforced by the `pre-commit` root-md allowlist guard and the `agent-files-check` CI workflow.
- **Path normalization**: all path comparisons must go through a single normalize utility. Windows UNC prefixes (`\\?\C:\`), backslash/forward-slash mismatches, and worktree `.git` file resolution have each caused subtle bugs in the past.
- **ONNX arena allocator**: uses `kNextPowerOfTwo` growth, never returns memory to OS. ~2GB memory during indexing is a known limitation. No local fix available until upstream fastembed exposes `OrtArenaCfg`.
- **Git worktrees**: `find_git_root` returns the worktree directory itself. Each worktree is a separate indexable repo. Groups can be used to search across worktrees of the same base repo.

## Runtime locations

- **Runtime dir**: `C:\Users\develterf\.local\bin\` — contains `codesearch.exe` and `helpers/csharp/scip-csharp.exe`. This is where `codesearch serve` runs from.
- **Build:** via `build.ps1` (repo root) — it self-heals the bare-flag quirk and sets `CARGO_TARGET_DIR` so `target/` stays outside the repo.
- **Build dir**: `target/release/` — lives **outside the repo** (set via `CARGO_TARGET_DIR`). For compilation only. Never run codesearch from this location.
- **Logs**: `~\.codesearch\logs\` — codesearch writes structured logs here during serve. Check these for startup errors, rebuild failures, and helper detection messages.

## Deploying to runtime

- `..\copy-to-common.ps1` — builds and copies **both** `codesearch.exe` and `scip-csharp.exe` to `~/.local/bin/` (the common execution dir). Use this to update the runtime binaries. **No `--release` builds — always dev/debug.**
- The C# helper is built via: `dotnet publish helpers/csharp/scip-csharp.csproj -r win-x64 --self-contained -c Release`
- Helper output must be **single-file only**: `scip-csharp.exe` (+ optional `.pdb`). The `.csproj` has `PublishSingleFile=true`.
- Do NOT copy framework DLLs, `BuildHost-*` dirs, or `.dll.config` files to the runtime location.

## Notes for agents

- **Never use the bundled `codesearch` binary to investigate this repo** (it is the project under development). Use codesearch MCP tools first for discovery (this repo is indexed as `codesearch-git`); `grep`/`Read` for exact refs, other git refs, or when MCP returns nothing.
- **Tests live in sibling `_tests.rs` files**, table-driven preferred over near-duplicate per-case fns.
- **Tests that set env vars must be `#[serial]`** and set them via `crate::testing::EnvRestore` — cargo runs tests as parallel threads of one process, so an unserialised `set_var` races every reader.
- **Never call `.canonicalize()`** — use `safe_canonicalize()`.
- **Windows transient rename errors** (os error 5/32/33 from AV/Search-Indexer races): classify with `is_transient_rename_error()` / `ServeState::is_db_locked_error` and wrap in a bounded retry. Never retry non-transient errors.
- **Counter-then-teardown races:** a background task tearing down state guarded by an in-flight counter must take the write lock BEFORE checking the counter and hold it across check + clear. Consumers increment the counter before acquiring the resource, so `counter == 0` under the write lock proves no consumer exists.

### LMDB rules

- **One `EnvOpenOptions::open()` per directory per process.** All access via `get_or_open_stores()` → `Arc<SharedStores>`; SCIP opens share a per-directory env (`get_or_open_shared_env`).
- **Open every env with `BASE_ENV_FLAGS`** (`src/lmdb_registry.rs`) — heed refuses to reopen one path with different options.
- **Commit, never drop, a txn whose DB handle you keep** — an aborted txn's DBI is closed by LMDB; using it later yields a bare `EINVAL`.
- **A dropped `TrackedEnv` must close via `prepare_for_closing()`** — heed's `OPENED_ENV` cache keeps a clone, so a plain drop never runs `mdb_env_close` and Windows keeps the files locked for the process lifetime (the `index rm` os-error-32 bug).

### Search errors must not become empty results

Never `unwrap_or_default()` a store error on a search path — "no results" and "store down" must stay distinguishable. This defect was re-introduced across sibling handlers in seven review rounds; it is a class, not a site:

- Render error chains with `{:#}`, never `{}`. Pass "not found" claims through `qualify_empty_result()`; never state a diagnosis you did not verify.
- The rule covers EVERY MCP handler: `find`, `get_chunk`, `explore`, `find_imports`, `find_dependents`, and the single-store `project=` paths.
- **Warnings channels must terminate** on every path that writes to them, and the read must be reachable from the last write.
- **Every `for store in stores` fan-out opens its `*_warnings` channel before the loop.** `Err(_)` over a store result is banned: bind it, render `{e:#}`, carry it. `MultiReadOutcome` is `#[must_use]`.
- **Take the channel as a parameter** — use `respond_with_items()` / `respond_with_object()`, which cannot be called without the channel.
- **New response shapes use the shared exits, not hand-rolled ones.** `serde_json::json!` renders `None` as an explicit `null`, so conditional keys must be *inserted*, not set.
- **Suppress `suggested_tool` when warnings are present.**
- **Verify a batch edit by re-running its detector over the whole file**, including the lines the edit added.
- **Before claiming a test pins a fix, reintroduce the defect and watch it fail.**
- **A caller-facing literal wrapped across lines needs a `\` continuation** — enforced by `tests/caller_facing_literals.rs`.

---

## Active feature branches (not yet merged)

| Branch | Description |
|---|---|
| *(none)* | |

---

## Changelog highlights (recent)

- **v1.3.37** — per-index embedding models end-to-end: serve queries, `POST /repos` and CLI index/stats/status honour the model each index records in its `metadata.json`; `serve --model` sets the default for newly created indexes; unrecorded indexes are queried with the built-in model plus a caller-facing warning; mid-rebuild indexes no longer report ready (PR #248)
- **v1.3.23–v1.3.36** — dependency + platform wave: rmcp 3.3, fastembed 6.1 + ort rc.13, tantivy 0.26, axum 0.8, ratatui 0.30 + crossterm 0.29, thiserror 2, notify 8, tree-sitter 0.27, dirs/sha2/scip/sysinfo refresh + dependabot (weekly); clears the open Aikido/RUSTSEC advisories
- **v1.3.19** — `find_impact` ambiguity envelope + `resolved_symbol`; partial-results `warnings`; C# symbol-key uniqueness (index v2.0)
- **v1.3.16** — REST `/find-impact` endpoint
- **v1.3.3** — federation hardening release

Older entries: see `CHANGELOG.md`.

---

## Plan — fix/stale-indexing-marker-handle-leak

Repo `BOIN.Aprimo` sat at 0 chunks for two days: a format-recovery rebuild wedged, `is_indexing`
evicted its stale marker, the reaper evicted the repo ("DB closed"), yet the task kept running and
kept the `Arc<SharedStores>` — so `.writer.lock` and the LMDB env stayed held and every write failed
with "Database is locked by another process".

1. `is_indexing`'s stale-marker eviction cancels the task registered in `index_tasks` (cooperative
   token cancel; the handle is still never aborted) so its store handles drop. — done
2. Format recovery reports a cancelled rebuild as a failure instead of logging "rebuild complete"
   over an empty index. — done
3. Idle eviction warns with the `lmdb_registry` holder list when the env is still open after evict. — done
4. Regression tests + CHANGELOG (1.4.4). — done
