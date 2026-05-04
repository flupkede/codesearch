# AGENTS.md — features/fixes-strict-scoping-and-reaper

## Goal

Address follow-up findings from the post-merge audit of `features/strict-scoping-and-reaper` (PR #42) and `features/serve-tui-standalone` (PR #43). The merged code is functionally correct, but two gaps remain:

1. **No automated tests** for the new Warm/Write state semantics, the zombie-proof reaper, or the new `/status` endpoint
2. **No HTTP timeouts** in the standalone TUI's reqwest calls — a hung serve would block the TUI indefinitely

This branch closes both gaps. No behaviour changes — only safety nets and tests.

---

## Background

The audit verified the following implementations are correct:

- `src/mcp/mod.rs::get_chunk` — scope_required check before resolve_routing in multi-repo mode
- `src/serve/mod.rs::get_or_open_stores` — slow path inserts `Warm` when touch=false (no FSW), `Write` when touch=true; `last_access` always updated
- `src/serve/mod.rs::status_handler` — uses `repo_statuses_lightweight()`, no DB opens
- `src/serve/tui_remote.rs::run_remote_tui_loop` — 1s polling, connection error counter, render layout
- `src/serve/mod.rs::run_tui_standalone` — TTY check + health check before launching TUI
- `src/cli/mod.rs::ServeAction` — `Tui` and `Start` dispatch correctly

The original AGENTS.md for strict-scoping-and-reaper specified 4 unit tests + 3 integration tests. **None of these were implemented.** That's the primary gap.

---

## Plan

### 1. Unit tests for state-machine correctness — `src/serve/mod.rs` test module

Add to the existing `#[cfg(test)] mod tests { ... }` block in `src/serve/mod.rs`. Use the test helpers already in that module (e.g. `make_test_state` if it exists, otherwise create a minimal one).

#### `test_slow_path_warm_when_touch_false`

Create a fresh `ServeState` with one alias registered. Call `get_or_open_stores(alias, false)`. Assert:
- `state.repos.get(alias)` is `RepoState::Warm { .. }`, not `RepoState::Write`
- `state.last_access.contains_key(alias)` is true
- No FSW task is spawned (`index_manager` is not present in Warm variant by definition)

#### `test_slow_path_write_when_touch_true`

Same setup, but call `get_or_open_stores(alias, true)`. Assert:
- `state.repos.get(alias)` is `RepoState::Write { .. }`
- `state.last_access.contains_key(alias)` is true

#### `test_reaper_evicts_warm_repo_after_idle`

Set up a state with one `Warm` repo whose `last_access` timestamp is older than `REPO_IDLE_TIMEOUT_SECS`. Call `evict_idle_repos()` directly. Assert:
- Repo state becomes `Closed` (or removed from `state.repos`, depending on current implementation)
- `last_access` entry is removed

#### `test_evicted_repo_reopen_via_fan_out_stays_warm`

Open repo with touch=true → `Write`. Force eviction. Reopen with touch=false. Assert:
- Final state is `Warm`, not `Write`
- `last_access` updated again

### 2. Integration tests — new file `tests/multi_repo_scope_required.rs`

These need a running serve mock or a way to invoke the MCP handlers directly. Look at how existing integration tests in `tests/` invoke handlers — likely via `CodesearchService` constructed with a mocked or test `ServeState`.

#### `test_get_chunk_requires_scope_in_multi_repo`

Register 2 repos in serve mode. Call `get_chunk(chunk_id=1)` without project/group. Assert response is the structured `scope_required` error (parse the returned text as JSON, check `error_code == "scope_required"`, `available_projects.len() == 2`).

#### `test_get_chunk_works_with_project_in_multi_repo`

Same setup. Call `get_chunk(chunk_id=<known>, project=<alias>)`. Assert the chunk content is returned, no scope error.

#### `test_status_endpoint_does_not_open_dbs`

Register 2+ cold repos in serve mode. Snapshot `state.repos` states (should all be `Closed` or absent). Call `status_handler` directly via axum's testing utilities, or call the underlying `repo_statuses_lightweight()` function. Assert:
- All repos are still in their pre-call state (no transitions to Open/Warm)
- Returned JSON has the expected shape: `version`, `repos[]`, `active_sessions`, `cpu_percent`

If axum testing is too heavy, test `repo_statuses_lightweight()` directly instead — that's the function `status_handler` delegates to.

### 3. Reqwest timeouts in TUI remote — `src/serve/tui_remote.rs` and `src/serve/mod.rs`

Replace bare `reqwest::get(url)` calls with a configured `reqwest::Client` that has a 2-second timeout.

In `tui_remote.rs::run_remote_tui_loop`:

```rust
let client = reqwest::Client::builder()
    .timeout(std::time::Duration::from_secs(2))
    .build()
    .map_err(|e| anyhow::anyhow!("reqwest client init failed: {}", e))?;

// inside the loop:
match client.get(&status_url).send().await {
    Ok(resp) if resp.status().is_success() => { ... }
    _ => { remote_state.connection_errors += 1; }
}
```

In `mod.rs::run_tui_standalone`:

```rust
let client = reqwest::Client::builder()
    .timeout(std::time::Duration::from_secs(2))
    .build()?;

match client.get(&health_url).send().await {
    Ok(resp) if resp.status().is_success() => {}
    Ok(_) => { ... }
    Err(_) => { ... }
}
```

Use 2 seconds — long enough for any healthy local serve to respond, short enough that a hung serve gives the user a clear error within reason.

### 4. (Optional, lower priority) `Indexing` lock_mode in `status_handler`

Currently `Open | Indexing → "write"` in the lock_mode mapping. If we ever want the TUI to distinguish active indexing from regular write mode, we'd need a third lock_mode value. **Skip this unless time permits** — it's cosmetic and the embedded TUI already handles `Indexing` distinctly via `status` not `lock_mode`.

### 5. Document group fan-out behaviour in `AGENTS.develop.md`

Add a small note to the "Multi-repo serve mode" section explaining that `get_chunk(group="X", chunk_id=N)` opens all repos in group `X` to find the chunk. This is intended behaviour but worth documenting so future agents don't try to "optimize" it away.

Suggested addition:

> **Group fan-out:** `get_chunk(group=X)`, `search(group=X)`, etc. open all repos in the group via `touch=false` (Warm only, no FSW). The reaper then evicts them after the idle timeout. This is by design — group queries inherently need access to all member repos.

---

## Files to modify

| File | Change |
|---|---|
| `src/serve/mod.rs` (test module) | Add 4 unit tests for slow-path Warm/Write semantics and reaper |
| `tests/multi_repo_scope_required.rs` (new) | Add 3 integration tests for scope_required, project routing, status endpoint |
| `src/serve/tui_remote.rs::run_remote_tui_loop` | Replace `reqwest::get` with `reqwest::Client` with 2s timeout |
| `src/serve/mod.rs::run_tui_standalone` | Same — `reqwest::Client` with 2s timeout |
| `AGENTS.develop.md` | Add group fan-out note + changelog entry for this branch |

---

## Quality gates

- [ ] `cargo check` clean
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` clean
- [ ] `cargo test --lib --bins` — all existing tests still pass, new unit tests pass
- [ ] `cargo test` — integration tests pass
- [ ] Manual: kill serve mid-poll while standalone TUI runs — TUI shows "Connection lost" within 2-3 seconds, not hangs
- [ ] Manual: `codesearch serve tui --url http://127.0.0.1:1` (unreachable port) — error within 2 seconds, not hang

---

## CHANGELOG (in `AGENTS.develop.md`)

Add under "Changelog highlights":

```
- **vX.Y.Z** — Test coverage for multi-repo scope enforcement and Warm/Write state machine; HTTP timeouts in standalone TUI prevent hangs on unresponsive serve
```

---

## Branch flow

```
# Already on features/fixes-strict-scoping-and-reaper (branched from develop)
# Implement, test, commit incrementally
git push -u origin features/fixes-strict-scoping-and-reaper

# When done:
# - Update AGENTS.develop.md (remove this branch from "Active feature branches" — it's not in the list yet, but add the changelog line)
# - PR features/fixes-strict-scoping-and-reaper → develop
```

---

## Done when

- [ ] 4 unit tests in `src/serve/mod.rs` for slow-path Warm/Write/reaper semantics
- [ ] 3 integration tests in `tests/multi_repo_scope_required.rs`
- [ ] Reqwest client with 2s timeout in both `run_remote_tui_loop` and `run_tui_standalone`
- [ ] `AGENTS.develop.md` updated with group fan-out note + changelog line
- [ ] All quality gates pass
- [ ] PR opened against `develop`

---

## Notes for OpenCode

This is primarily a test-coverage and defensive-coding branch. The core logic is already correct — don't change behaviour, just add safety nets.

The trickiest part is the integration tests: figure out how to invoke MCP handlers without a full serve process. Look for existing patterns in `tests/` first. If those use a real `ServeState` with TempDir-backed databases, follow that pattern. If no integration tests exist at all and unit tests in `src/serve/mod.rs` are the established style, put the integration-style tests there too rather than fighting the test infrastructure.

For the reqwest timeout: do NOT use `reqwest::Client::new()` and then add timeout per-request — that doesn't work. The timeout must be set on the Client builder. Verify by killing serve mid-test or pointing at a port where nothing listens.
