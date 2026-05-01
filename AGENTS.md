# AGENTS.md — features/fixes branch plan

## Branch: `features/fixes`
**Base:** `develop`
**Goal:** Fix idle eviction bug + improve search quality to reduce agent grep fallback

---

## Fix 1: Idle eviction — `get_or_open_stores` touches ALL repos on fan-out

### Problem

`get_or_open_stores()` calls `touch_access()` unconditionally (lines 456, 470 in `src/serve/mod.rs`).
When `get_chunk` is called without `project`/`group` (`allow_unscoped=true`), `resolve_repo_stores_multi`
fans out to ALL repos via `get_or_open_stores()` — resetting the idle timer on every repo.

Result: repos are never idle, reaper never evicts. The 30-minute timeout is effectively disabled
whenever any agent uses `get_chunk` without explicit project scope.

Same issue affects `status` tool with explicit project/group (goes through `get_or_open_stores`),
though the unscoped `status` path uses `repo_statuses_lightweight()` which is safe.

### Fix: Add `touch: bool` parameter to `get_or_open_stores`

**File:** `src/serve/mod.rs`

1. Change signature: `pub(crate) async fn get_or_open_stores(&self, alias: &str, touch: bool)`
2. Only call `self.touch_access(alias)` when `touch == true`
3. Update all call sites:
   - `warmup_repo` (line 456) → `touch: false` (warmup should NOT reset idle timer)
   - `get_or_open_stores` fast path (line 470) → keep `touch: true` (direct query access)
   - `resolve_repo_stores_multi` fan-out (line 3113 in `src/mcp/mod.rs`) → `touch: false`
   - `resolve_repo_stores_multi` single project (line 3130) → `touch: true`
   - `resolve_repo_stores_multi` group members (line 3141) → `touch: false`
   - Lazy FSW transition (line ~487) → `touch: true` (first real query)
   - `spawn_fsw_for_warm` (line ~722) → `touch: false`
   - `reindex_handler` (lines 1166, 1211) → `touch: true`
   - Test call sites → `touch: true`

4. After `get_chunk` candidate detection resolves to a single repo, explicitly call
   `serve_state.touch_access(&resolved_alias)` for just that repo.

5. After group fan-out search completes, touch only repos that contributed results
   (or touch all group members — acceptable since the agent explicitly requested the group).

### Validation
- `cargo check && cargo clippy --all-targets -- -D warnings`
- `cargo test --lib`
- Manual: start serve with 3+ repos, call `get_chunk` without project, verify reaper log
  shows idle ages increasing (not resetting) for untouched repos

---

## Fix 2: Search quality — reduce agent grep fallback

### Problem

Agents fall back to `grep` when `codesearch_search` returns poor or zero results.
Root causes:

1. **Top-N cutoff too aggressive** — retrieval pool is `limit * 3`, fusion drops relevant results
2. **Exact identifier boost too weak** — `EXACT_MATCH_RRF_K = 5.0` doesn't sufficiently
   prioritize exact code matches over semantic similarity
3. **No auto-fallback** — when semantic search returns few results, no automatic literal retry
4. **minilm-l6 weak on code** — embedding model is NL-trained, code identifiers get poor vectors.
   Not fixable without model change, but compensated by stronger FTS fusion.

### Fix 2a: Increase retrieval pool

**File:** `src/mcp/mod.rs`

Change all `limit * 3` to `limit * 5` in the semantic search pipeline.
This gives the RRF fusion more candidates to work with, reducing the chance
that a relevant result falls outside the retrieval window.

Affected locations (all in `src/mcp/mod.rs`):
- Line 3698: `store.search(&query_embedding, limit * 3)` → `limit * 5`
- Line 3753: `fts_store.search(&request.query, limit * 3, ...)` → `limit * 5`
- Line 3864: `fts_store.search(&request.query, limit * 3, ...)` → `limit * 5`
- Line 3925: `store.search(&query_embedding, limit * 3)` → `limit * 5`
- Line 3955: `fts_store.search(&request.query, limit * 3, ...)` → `limit * 5`
- Line 4108: `fts_store.search(&request.query, limit * 3, ...)` → `limit * 5`

Also in `src/search/mod.rs` (CLI search path) — same pattern.

Leave `search_exact` at `limit * 2` (exact matches are already high-precision).
Leave `search_phrase` at `limit * 3` (phrase search is already precise).

### Fix 2b: Stronger exact identifier boost

**File:** `src/rerank/mod.rs`

Change `EXACT_MATCH_RRF_K` from `5.0` to `2.0`.

Lower K = steeper rank curve = exact matches get proportionally higher RRF scores.
At K=5, an exact match at rank 1 gets score `1/(5+1) = 0.167`.
At K=2, an exact match at rank 1 gets score `1/(2+1) = 0.333` — 2x stronger signal.

This ensures that when an agent searches for `"evict_idle_repos"`, the chunk containing
that exact identifier dominates the fusion result even if the embedding similarity is low.

### Fix 2c: Auto-fallback to literal search

**File:** `src/mcp/mod.rs`, in `semantic_search()` (line ~3620)

After the hybrid search completes and results are built:

```rust
// If semantic/hybrid returned fewer than 3 results and query looks like code,
// auto-fallback to literal search and merge results.
if results.len() < 3 && has_identifiers {
    // Try literal FTS search as fallback
    let literal_results = fts_store.search(&request.query, limit, None)?;
    // Deduplicate by chunk_id and append
    for lr in literal_results {
        if !results.iter().any(|r| r.id == lr.chunk_id) {
            // Convert FtsResult to SearchResult and append
        }
    }
}
```

Implementation details:
- Only trigger when `results.len() < 3` AND `has_identifiers` (code-like query)
- Use `with_fts_store_read_for` to run the fallback FTS search
- Deduplicate by `chunk_id` before merging
- Cap total results at `limit`
- Log when fallback triggers: `tracing::debug!("Auto-fallback: semantic returned {} results, trying literal", results.len())`

### Fix 2d: Increase `search_exact` retrieval for identifiers

**File:** `src/mcp/mod.rs`

Change `search_exact(ident, limit * 2, ...)` to `search_exact(ident, limit * 3, ...)`
in the identifier boost paths (lines 3762, 3876, 3968, 4120).

More exact candidates = better chance the right chunk survives RRF fusion.

### Validation
- `cargo check && cargo clippy --all-targets -- -D warnings`
- `cargo test --lib`
- Manual test queries that previously required grep fallback:
  - `codesearch search "evict_idle_repos"` — should find the function
  - `codesearch search "touch_access"` — should find the method
  - `codesearch search "Database cleared"` — should find the log message (already fixed by AND mode)
  - `codesearch search "EXACT_MATCH_RRF_K"` — should find the constant

---

## Execution order

1. **Fix 1** — idle eviction (`touch` parameter)
2. **Fix 2a** — retrieval pool `limit * 5`
3. **Fix 2b** — `EXACT_MATCH_RRF_K` = 2.0
4. **Fix 2c** — auto-fallback to literal
5. **Fix 2d** — `search_exact` retrieval `limit * 3`
6. Validate all together
7. Commit

## Commits

One commit per fix, or group 2a-2d into a single "search quality" commit.
Prefer: 2 commits total (Fix 1 + Fix 2).
