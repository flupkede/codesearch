# AGENTS.md — features/strict-scoping-and-reaper

## Goal

Fix two related bugs in multi-repo serve mode that currently cause repos to stay
open in `RepoState::Write` indefinitely, even after long idle periods, leaving
file system watchers and LMDB envs running for repos the user is not actively
using.

The user observed: a single tool call with an explicit `project=...` parameter
caused **all** registered repos to jump to "ready" status with "write" lock in
the TUI. Hours later, only the repo that was actually queried (via explicit
`project=`) was evicted by the idle reaper. The others remained in Write state
forever.

This branch addresses the root cause and prevents recurrence.

---

## Background — the bug

Two interacting issues:

### Issue 1: get_chunk fan-out without scope

`get_chunk(chunk_id=N)` without `project` or `group` is currently tolerant in
multi-repo mode: it opens **all** registered repos to scan for the chunk_id.
The justification was historical convenience for agents who forget to pass
`project=`. In practice this is inconsistent with the other tools (`search`,
`find`, `explore` — all require scope) and is the trigger for the cascade.

### Issue 2: zombie repos in Write state

When a fan-out call (or any reopen via `get_or_open_stores(alias, touch=false)`)
hits the slow path of `get_or_open_stores`, the slow path inserts directly into
`RepoState::Write` regardless of `touch` — file watcher starts, but
`last_access` is **not** updated because `if touch { self.touch_access(alias); }`
is gated.

Result: repo is in `Write` (FSW running, resources held) but not tracked in
`last_access`. The idle reaper iterates `last_access` to find candidates for
eviction, so it never sees these zombie repos. They stay alive until process exit.

The repo that the user actually queried via `project=...` does end up in
`last_access` (touch=true path), so it evicts normally after 30 min idle.

---

## Architecture

Four changes, ordered from cause-elimination to defense-in-depth:

### 1. Strict scoping for get_chunk

`get_chunk` without `project` or `group` in multi-repo serve mode must return
a `scope_required` error, exactly like `search`, `find`, and `explore` already do.

Match the existing scope_required error shape:
```json
{
  "error": "scope_required",
  "message": "...",
  "available_projects": [...],
  "available_groups": [...],
  "hint_for_agent": "..."
}
```

The hint should say something like: "When calling get_chunk in multi-repo mode,
specify the project the chunk belongs to. The chunk_id you received from
search/find/explore is local to that project."

Single-repo serve mode (one alias registered) keeps current behaviour — no
scope needed.

### 2. status kind=projects must not open DBs

`index_status_impl` already has a lightweight early-return path when both
`project` and `group` are `None` — it uses `repo_statuses_lightweight()` which
reads from the DashMap without opening any DB. **Verify** this path is also
taken for `kind="projects"` requests, not only for `kind="index"`.

If `status(kind="projects")` currently still goes through `resolve_routing`
with `allow_unscoped=true` (and thus fan-out), reroute it to the lightweight
path. The projects listing is pure inventory — no DB access required.

`config_snapshot()` + `repo_statuses_lightweight()` together give: list of
aliases, their current state (Open/Warm/Closed/etc.), groups membership.
That's everything `status(kind="projects")` needs.

### 3. Slow path opens in Warm when touch=false

In `ServeState::get_or_open_stores`, the slow path currently does:

```rust
self.repos.insert(
    alias.to_string(),
    RepoState::Write { stores, index_manager, cancel_token },
);
if touch { self.touch_access(alias); }
```

Change to:

```rust
let new_state = if touch {
    // explicit query — start FSW, full Write mode
    RepoState::Write { stores, index_manager, cancel_token }
} else {
    // fan-out / candidate detection — Warm only, no FSW
    RepoState::Warm { stores: stores.clone() }
};
self.repos.insert(alias.to_string(), new_state);
```

This restores the original Warm/Write semantics: Warm = DB open, vector index
ready, no FSW. Write = Warm + FSW + reindex pipeline. Fan-out callers
(get_chunk candidate scan in single-repo mode, future tools that legitimately
fan out) get Warm; explicit project queries get Write.

### 4. last_access always updated on open

In the slow path (and the fast-path Warm→Write transition), update
`last_access` **unconditionally** when a repo is being opened or transitioned
to Write. Currently it's gated on `touch`.

Rationale: any repo that is in `self.repos` consumes resources and should be
visible to the reaper. The `touch` parameter was meant to control "should this
count as a real query for idle tracking" but in practice it leaves zombie
state. Decoupling: always track in last_access, let the reaper evict based
on time elapsed since last touch.

```rust
// in slow path
self.repos.insert(alias.to_string(), new_state);
self.touch_access(alias);  // ALWAYS, regardless of touch param
```

For the fast path (already-opened repo): keep current behaviour — only update
last_access when touch=true. Fan-out reads of an already-open repo shouldn't
keep resetting the clock; only real queries should.

---

## Files to modify

| File | Change |
|---|---|
| `src/mcp/mod.rs` — get_chunk impl | Add scope_required check at the top: if multi-repo mode and no project/group, return scope_required error |
| `src/mcp/mod.rs` — index_status_impl | Verify `kind="projects"` takes the lightweight path; reroute if not |
| `src/serve/mod.rs` — `get_or_open_stores` slow path | Insert as `Warm` when touch=false, `Write` when touch=true; always update last_access |
| `src/serve/mod.rs` — `get_or_open_stores` fast path Warm→Write | Always update last_access in the transition (not just on touch=true) — but the existing fast-path touch-gating for already-open repos stays as-is |
| `tests/...` (new or existing) | Tests covering: get_chunk without scope errors out; fan-out leaves repos in Warm; zombie can't recur |

---

## Tests

Unit tests in `src/serve/mod.rs` test module:

- `test_slow_path_warm_when_touch_false` — open a fresh repo with touch=false, assert state is `Warm` and last_access has an entry
- `test_slow_path_write_when_touch_true` — same with touch=true, assert state is `Write` and FSW spawned
- `test_evicted_repo_reopen_via_fan_out_stays_warm` — evict a repo, then call get_or_open_stores(alias, false), assert Warm and reaper-visible
- `test_reaper_evicts_warm_repo_after_idle` — set up a Warm repo with old last_access, run reaper, assert evicted

Integration test in `tests/`:

- `test_get_chunk_requires_scope_in_multi_repo` — register 2 repos in serve mode, call get_chunk without project/group, assert scope_required error
- `test_get_chunk_works_with_project_in_multi_repo` — same setup, call with project, assert returns chunk content
- `test_status_projects_does_not_open_dbs` — register 2 cold repos, call status(kind=projects), assert all repos still in their pre-call state (no DBs opened)

---

## Quality gates

- [ ] `cargo check` clean
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` clean
- [ ] `cargo test --lib --bins` all pass
- [ ] Manual: register 5+ repos in serve mode, start serve, call `get_chunk(chunk_id=1)` without project — assert scope_required error
- [ ] Manual: call `status(kind="projects")`, observe TUI — assert no repos transition to Open/Warm states (stay in their pre-call state)
- [ ] Manual: query a single repo with `search(project="X")`, observe X transitions to Write, others stay untouched
- [ ] Manual: leave running for 35 minutes without further queries — assert X transitions to Closed via reaper, no zombie repos remain in Write

---

## CHANGELOG

```markdown
### Fixed

- Multi-repo serve no longer leaks repos into permanent `Write` state when a
  fan-out call (e.g. unscoped `get_chunk`) opened them. Repos opened via
  fan-out now correctly stay in `Warm` state and are evicted by the idle
  reaper after the configured timeout.
- `last_access` is now updated on every repo open, eliminating zombie repos
  that were invisible to the idle reaper.

### Changed

- `get_chunk` in multi-repo serve mode now requires `project` or `group`,
  consistent with `search`, `find`, and `explore`. Calls without scope return
  a structured `scope_required` error listing available projects and groups.
  Single-repo serve mode is unaffected.
- `status(kind="projects")` no longer opens any database files — it now
  returns the inventory purely from the DashMap state, as already done for
  `status(kind="index")` without scope.
```

---

## Branch flow

```powershell
# already on features/strict-scoping-and-reaper (branched from develop)
# implement, test, commit incrementally
git push -u origin features/strict-scoping-and-reaper

# When done: PR features/strict-scoping-and-reaper → develop
```

---

## Done when

- [ ] `get_chunk` without scope returns `scope_required` in multi-repo mode
- [ ] `status(kind="projects")` does not open any DBs
- [ ] Slow path of `get_or_open_stores` inserts `Warm` when touch=false, `Write` when touch=true
- [ ] `last_access` is updated on every open (slow path) and every Warm→Write transition (fast path)
- [ ] Tests cover the four behaviours above
- [ ] Manual test: 5+ repos, single project query, only that repo goes to Write, all others stay Warm or untouched
- [ ] Manual test: 35 min idle → all queried repos closed, no zombies
- [ ] CHANGELOG entry written
- [ ] PR opened against `develop`

---

## Notes for OpenCode

This is primarily a state-machine correctness fix. Read `src/serve/mod.rs`
carefully — the `RepoState` enum, `get_or_open_stores` (multi-part chunk),
and `evict_idle_repos` together define the lifecycle. The bug lives at the
boundary between "open a repo" and "track for eviction".

The fast path of `get_or_open_stores` for an already-opened Warm repo with
`touch=true` does the Warm→Write transition via `spawn_fsw_for_warm`. That
code already exists and is correct — the change there is only to ensure
`last_access` is updated alongside the transition (currently it relies on
the `if touch { self.touch_access(alias); }` line at the top of the fast
path, which works for the fast path but not for the slow path).

The slow path is where the zombie originates. Make sure the new code does
NOT spawn FSW when inserting as Warm — that's what `spawn_fsw_for_warm` is
for, and it should only run on the Warm→Write transition.

When verifying the fix manually, the TUI is your friend: lock column shows
"read" for Warm, "write" for Write, "—" for Closed. After the fix, a
fan-out call should leave most repos in "read" (Warm), not "write" (Write).
