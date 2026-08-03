# Worklog — watcher reindex / TUI visibility

- **Branch:** `fix/watcher-reindex-tui-visibility`
- **Base SHA:** `09b451aaa7372285f76d50fad71a035aa40e4fd5` (develop)
- **Scope:** Make watcher-triggered reindexes visible in the serve TUI and rebuild
  C#/TypeScript symbols on branch switch. Three user-reported gaps:
  A) branch switch never rebuilds symbols (find_impact goes stale);
  B) the C# indicator never shows "Indexing" during a watcher rebuild;
  C) symbol-rebuild log lines lack a repo label;
  plus (gap #1) ordinary text-batch reindexes never signal "Indexing" in the TUI.
- **Status:** ✅ COMPLETE — all 3 stages + DRY refactor committed; every per-stage
  review and the final full-branch review PASSED. Not pushed (awaiting user).
- **Latest test result:** `cargo fmt --all --check`, `cargo check --all-targets`,
  `cargo clippy --all-targets -- -D warnings` clean; 609 lib tests pass.
- **Final review:** ✅ PASS on full diff `09b451a..78c9310` (holistic signal/label
  balance, single-source-of-truth rebuild helper, no stale 2-arg notifier sites).

## Root cause (from code, not memory)

| Watcher path | Signalled `indexing_cb` (general TUI "Indexing")? | Set `CSharpIndexStatus::Indexing`? | Rebuilt symbols? |
|---|---|---|---|
| Text batch flush (`process_batch_with_stores`) | ❌ no (gap #1) | n/a | n/a |
| Branch switch | ✅ yes (text refresh only) | ❌ no | ❌ no — discards `.cs/.ts` buffers (gap A) |
| `.cs` debounce | ✅ yes | ❌ no (gap B) | ✅ yes (incremental) |
| `.ts` debounce | ✅ yes | n/a (no TS notifier) | ✅ yes (full) |

The serve-layer helper `trigger_symbol_rebuild` (src/serve/mod.rs) already sets
`CSharpIndexStatus::Indexing` + `begin_indexing` + Full rebuild, but the watcher
in `IndexManager` cannot reach it — it only holds the two callbacks.

## Stages

### Stage 1/3 — Text-batch TUI visibility + repo-label logging (gap #1 + Fix C text paths)
- Commit: `3d43993da4d40d4711a6bb9d443bee3019e21ffe` — review: ✅ PASS (no remarks).
- Wrapped the FSW text-batch flush in `indexing_cb(true/false)` so ordinary file
  edits surface as "Indexing" in the TUI (the `IndexingStatusCallback` doc already
  claimed it fired on "batch flushes"; it never did).
- Added a `repo_label` (repo directory name = serve alias) to the watcher task and
  interpolated it into batch-flush and branch-change log lines.
- Files: `src/index/manager.rs`.

### Stage 2/3 — C# indicator shows "Indexing" during watcher rebuild (Fix B)
- Commit: `2dbafa3d0b8cd4f6996267b40c2c6806cb769121` — review: ✅ PASS (no remarks).
- Refactored `CSharpRebuildNotifier` from `Fn(bool, Option<String>)` to a 3-state
  `SymbolRebuildSignal { Started, Succeeded, Failed(String) }`. The watcher emits
  `Started` right after the applies/available gate, so `make_csharp_notifier` sets
  `CSharpIndexStatus::Indexing` for the rebuild duration (was Ready/Error only).
- Guards (`!applies_to`, `!is_available`) return BEFORE `Started`, so the indicator
  is never left stuck on `Indexing`.
- Also labelled every C# rebuild log line with `repo_label`; refreshed two stale
  callback doc comments.
- Files: `src/index/manager.rs` (new file: no), `src/index/mod.rs`, `src/serve/mod.rs`.

### Stage 3/3 — Branch-switch symbol rebuild (Fix A)
- Commits: `928273d6ee0e66f6f66f64fdcc8126a2e063919b` (feature) —
  review: ⚠️ PASS WITH REMARKS (1 Important: duplicated full-rebuild block);
  `a5f66c819d45bf0c9d49d74293ee299e5f06f9e9` (remark fix) — extracted
  `IndexManager::run_full_rebuild_logged`; re-review ⚠️ PASS WITH REMARKS
  (one 4th copy left in the `.ts` path); `78c9310aa5f45b452d0929a586a96b7fb8a4b6cc`
  (fold-in) — routed the `.ts` debounce rebuild through the same helper →
  single source of truth for all four full-rebuild paths. 609 lib tests pass.
- Added `IndexManager::spawn_branch_change_symbol_rebuild(...)`: after the
  branch-change text refresh, a fire-and-forget `spawn_blocking` runs a
  `RebuildScope::Full` rebuild for every applicable+available language (C# + TS).
  Full scope is correct — a branch switch rewrites arbitrary files, so no
  incremental scope can be computed.
- Toggles the general `indexing_cb` label around the whole rebuild (only when a
  language actually applies → no TUI flash otherwise); C# also drives the
  `SymbolRebuildSignal` indicator.
- Files: `src/index/manager.rs`.

## Follow-ups / notes
- **Deletions-only `.cs` debounce bug (pre-existing, found in Stage 2 review):**
  when only `.cs` deletions are buffered (no modifications), the debounce path
  builds empty `groups`/`ungrouped`, skips both the fallback and the per-group
  loop, and emits `Started`→`Succeeded` WITHOUT running any rebuild — so the
  forwarded `cs_deleted` set is never purged from LMDB and deleted symbols
  linger. `manager.rs` grouped `.cs` path. Not fixed here (out of scope); a Full
  rebuild (or `Files{changed:[], deleted}`) when `groups.is_empty() && !cs_deleted.is_empty()`
  would fix it. Branch-switch deletions ARE now handled (Stage 3 Full rebuild).
- TypeScript watcher path updates no TUI symbol status (no TS notifier). Out of
  scope this iteration; candidate follow-up.
- Watcher symbol-rebuild paths have no per-repo mutex guard (already a tracked
  follow-up); concurrent rebuilds on the same repo are benign (alias-keyed).
  Rapid successive branch switches could overlap Full rebuilds — same tradeoff.

<!-- sync: trello-card= ; ado-org= ; ado-project= ; ado-workitem= ; ado-comment= -->
