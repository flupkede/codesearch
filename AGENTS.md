# AGENTS.md — codesearch

_Last updated: 2026-09-03 (prose cleanup — rules unchanged, narratives live in CHANGELOG.md)_

## Current state

- **Version:** `Major.Minor.Patch` (semver). Patch auto-bumps +1 on every PR merged to `develop` (`.github/workflows/bump-develop.yml`); minor bumps manually at release (`scripts/bump-version.sh --type minor`, resets patch→0). Per-commit uniqueness: `build.rs` `+<commit_count>`. See `RELEASING.md`.
- **Validation:** `cargo check` for iteration → `cargo clippy --all-targets -- -D warnings` → `cargo test --lib --bins` before a branch is done. No `--release` builds during the fix loop.
- **Deploy:** `..\copy-to-common.ps1` builds + copies both binaries to `~/.local/bin/`. Stop serve first — a running `codesearch.exe` is file-locked on Windows.

## Implemented features

Narratives live in `CHANGELOG.md`; this list is orientation only:

- **Federation** — `codesearch remote add/rm/list/mounts`; `@peer` group fan-out; mounted projects addressable as `project=<peer>/<alias>`; TUI mount management with live status. Remote write verbs (`add`, `reindex --force`) need a read-write peer; `rm` is not durable across a cold start.
- **Cloud indexer-job split** — heavy build job uploads a snapshot, light serve restores it. DOCS repos are `repo_read_only` in `repos.json`; only `custom-kb` stays writable with a bounded incremental reindex per KB `git pull`.
- **Language coverage** — 17 tree-sitter grammars (table in README). `find_impact` has SCIP symbol precision for C# (bundled `scip-csharp`, resident helper via its `serve` subcommand) and TypeScript (`npx scip-typescript`). Protobuf is text-aware chunking only.
- **`find_impact` hardening** — internal budget with a structured busy answer; on `busy: true` sleep `retry_after_seconds` and retry the SAME call (busy is progress, never a reason to fall back to text search). Results carry `index_head_sha` vs `current_head_sha`: drift is surfaced, never auto-reindexed.

## ⚠️ Design constraint — never poll a federated peer

**A federated peer is NEVER contacted on a timer.** Each poll wakes the peer's scale-to-zero replica, which then self-warms its full idle window (measured ~50% duty cycle from zero searches). Shipped twice (PR #181/#184), reverted twice — do not re-attempt. Local repos may be polled in the background; peers are contacted only by a real federated tool call (activity poke) or the explicit TUI `i` overlay. The TUI discovery tick is config-only (zero HTTP).

## Open TODOs

- **Verification only:** live two-cold-start id comparison on the cloud peer — confirm the custom-kb chunk-id fix removed the production symptom after the next redeploy.
- **Low severity:** literal-mode snippets for markdown chunks sometimes show the chunk's opening line instead of the matched line (the `match_info.unwrap_or_else` fallback), making true hits look like false negatives.

## Branching & PR workflow

- **Integration branch = `develop`** (default branch `master` receives releases only). ALL PRs target `develop`: pass `--base develop` to `gh pr create`, verify with `gh pr view <n> --json baseRefName`.
- **Merge style:** feature/fix → develop = merge commits (`--merge`); develop → master release PR = **squash**, with `--body "$(scripts/release-coauthors.sh)"` so contributors stay credited on the default branch.
- **Review requirement** is a repo ruleset; as owner, override with `gh pr merge <n> --merge --admin`.
- **CHANGELOG.md** is CI-checked (visible, not blocking): add an entry under the pending-version heading, or label the PR `no-changelog` for pure tooling/docs churn.
- **Release squashes regress the merge-base** → a release PR may fail with "cannot be cleanly created" even when content is fine. Fix: cut `release/vX.Y.Z` off develop, run `git merge -s ours origin/master` in it (strategy, not option), verify the diff vs master is the intended release delta, PR that into master. Never merge master into develop directly.

## Root file hygiene

Root keeps only the allowlisted markdown: `AGENTS.md`, `AGENTS.develop.md`, `CLAUDE.md`, `README.md`, `README_CSharp.md`, `CHANGELOG.md`, `RELEASING.md`. Any other markdown goes to gitignored `.docs/`. Enforced by the pre-commit hook.

## Notes for agents

- **Runtime:** `C:\Users\develterf\.local\bin\` — `codesearch.exe` + `helpers/csharp/scip-csharp.exe`. Build via `build.ps1` (target dir outside the repo via `CARGO_TARGET_DIR`; it self-heals the bare-flag quirk).
- **Tests live in sibling `_tests.rs` files**, table-driven preferred over near-duplicate per-case fns.
- **Tests that set env vars must be `#[serial]`** and set them via `crate::testing::EnvRestore` — cargo runs tests as parallel threads of one process, so an unserialised `set_var` races every reader.
- **Never call `.canonicalize()`** — use `safe_canonicalize()`.
- **Windows transient rename errors** (os error 5/32/33 from AV/Search-Indexer races): classify with `is_transient_rename_error()` / `ServeState::is_db_locked_error` and wrap in a bounded retry. Never retry non-transient errors.
- **Counter-then-teardown races:** a background task tearing down state guarded by an in-flight counter must take the write lock BEFORE checking the counter and hold it across check + clear. Consumers increment the counter before acquiring the resource, so `counter == 0` under the write lock proves no consumer exists.
- **Tooling:** never use the bundled `codesearch` binary to investigate this repo (it is the project under development). Use codesearch MCP tools first for discovery (this repo is indexed as `codesearch-git`); `grep`/`Read` for exact refs, other git refs, or when MCP returns nothing.

### LMDB rules

- **One `EnvOpenOptions::open()` per directory per process.** All access via `get_or_open_stores()` → `Arc<SharedStores>`; SCIP opens share a per-directory env (`get_or_open_shared_env`).
- **Open every env with `BASE_ENV_FLAGS`** (`src/lmdb_registry.rs`) — heed refuses to reopen one path with different options.
- **Commit, never drop, a txn whose DB handle you keep** — an aborted txn's DBI is closed by LMDB; using it later yields a bare `EINVAL`.
- **A dropped `TrackedEnv` must close via `prepare_for_closing()`** — heed's `OPENED_ENV` cache keeps a clone, so a plain drop never runs `mdb_env_close` and Windows keeps the files locked for the process lifetime (the `index rm` os-error-32 bug).

### Search errors must not become empty results

Never `unwrap_or_default()` a store error on a search path — "no results" and "store down" must stay distinguishable. This defect was re-introduced across sibling handlers in seven review rounds; it is a class, not a site:

- Render error chains with `{:#}`, never `{}` — plain `{}` hides the actual fault. Pass "not found" claims through `qualify_empty_result()`; never state a diagnosis you did not verify.
- The rule covers EVERY MCP handler: `find`, `get_chunk`, `explore`, `find_imports`, `find_dependents`, and the single-store `project=` paths.
- **Warnings channels must terminate** on every path that writes to them — a `warnings` response field or a `qualify_empty_result()` call — and the read must be reachable from the last write (an early-return arm can orphan everything after it).
- **Every `for store in stores` fan-out opens its `*_warnings` channel before the loop.** `Err(_)` over a store result is banned: bind it, render `{e:#}`, carry it. `MultiReadOutcome` is `#[must_use]`; reading `.results` while discarding `.failures` is `unwrap_or_default()` under a new name.
- **Take the channel as a parameter**, not as a field the handler fills in — use `respond_with_items()` / `respond_with_object()`, which cannot be called without the channel. A test that builds the response struct itself cannot see a field happen.
- **New response shapes use the shared exits, not hand-rolled ones.** `serde_json::json!` renders `None` as an explicit `null`, so conditional keys must be *inserted*, not set.
- **Suppress `suggested_tool` when warnings are present** — never suggest a retry against a store you know is down.
- **Verify a batch edit by re-running its detector over the whole file**, including the lines the edit added. Green intent is not a green post-condition.
- **Before claiming a test pins a fix, reintroduce the defect and watch it fail.** A green suite over a restored defect is the only proof that matters; note `serde_json::Map` sorts keys (no `preserve_order`), so round-trip through `to_value` silently re-orders.
- **A caller-facing literal wrapped across lines needs a `\` continuation** — enforced by `tests/caller_facing_literals.rs`.
