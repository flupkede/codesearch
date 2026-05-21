# AGENTS.md — features/symbol-references

## Goal

Add symbol-aware reference lookups to codesearch via `find_impact` MCP tool. Returns file/line-precise references so agents can plan refactors with IDE-level accuracy. MVP is **C# only**; architecture is language-agnostic through per-language `SymbolIndexer` adapters.

## Implemented Features

- **`find_impact` MCP tool** — transitive call-sites for a symbol (name or position-based), C# via `scip-csharp`
- **`scip-csharp` helper** — .NET 10 CLI wrapping Roslyn; `index` (definitions only), `find-refs` (lazy per-symbol), `batch-find-refs` (Phase 3 pre-warm)
- **Opt 1 — external-type filter** — skips framework/NuGet types, 10-100× fewer symbols on large solutions
- **Opt 2 — lazy reference resolution** — rebuild stores definitions only; refs resolved on demand and cached in `scip_ref_cache`
- **Opt 3 — incremental merge** — `RebuildScope::Files` uses position index as reverse map for stale symbols, merges new defs (partial-class safe)
- **O(1) lookups** — `scip_positions` (file:line → keys), `scip_simple_names` (identifier → keys)
- **Bincode schema versioning** — version byte prefix on all LMDB payloads; JSON version validation rejects non-`1.0`
- **Backward compat** — pre-Opt2 LMDB indexes (refs in `scip_symbols`) still work via `has_legacy_refs` check
- **Shared `SymbolIndexerRegistry`** — `Arc<Registry>` in `ServeState`, `CodesearchService`, `IndexManager`; no per-request instantiation
- **`.cs` watcher** — 60s debounce triggers automatic symbol rebuild; files grouped by .csproj for incremental rebuild
- **Sequential startup phases** — Phase 1 text/vector warmup, Phase 2 C# SCIP (Semaphore-gated, default concurrency 2), Phase 3 batch ref pre-warm
- **`repos_meta` tracking** — `RepoMeta` persisted in `repos.json` with debounced save (10s window)
- **TUI C# indicator** — green `C#·` ready, yellow `C#…` indexing, red `C#!` error; footer shows helper availability
- **Selective ref cache invalidation** — incremental rebuilds only purge refs for affected symbols
- **`index symbol` CLI** — symbol-only rebuild; `--symbols` flag on `index -f` for combined text+symbol rebuild
- **SCIP LMDB map_size 512 MB** — override with `CODESEARCH_SCIP_LMDB_MAP_MB` (virtual address space only)
- **CI** — `csharp_helper_integration` cargo feature + `csharp-integration-tests` GitHub Actions job
- **`-with-csharp` release variants** — 6 archives (3 plain + 3 with self-contained helper)

## Architecture

### Per-language adapter pattern

`src/symbols/` hosts the adapter layer:

- `mod.rs` — `SymbolIndexer` trait + `SymbolIndexerRegistry` dispatch
- `csharp.rs` — C# adapter (rebuild, find_references, find_references_by_position)
- `scip_parse.rs` — JSON parser for scip-csharp output

### LMDB tables

| Table | Key | Value |
|---|---|---|
| `scip_symbols` | full SCIP key | `[v1, bincode(Vec<StoredReference>)]` — **definitions only** after Opt 2 |
| `scip_positions` | `<file>:<line>` (forward-slash) | `[v1, bincode(Vec<String>)]` |
| `scip_simple_names` | last segment of canonical symbol | `[v1, bincode(Vec<String>)]` |
| `scip_ref_cache` | full SCIP key | `[v1, bincode(Vec<StoredReference>)]` — lazy-resolved references |
| `scip_meta` | `last_rebuild_ts`, `symbol_count` | `Str` |

### Helper detection lookup order

1. `CODESEARCH_SCIP_CSHARP` env var
2. `<codesearch-exe-dir>/helpers/csharp/scip-csharp[.exe]`
3. `$PATH`

Missing helper disables `find_impact` for C# only — all other features keep working.

### Startup phases

| Phase | What | Trigger |
|---|---|---|
| Phase 1 | Sequential text/vector warmup | `run_phase_1_warmup_all()` |
| Phase 2 | C# SCIP definitions-only rebuild | `run_phase_2_csharp_scip()`, gated by `Semaphore(CSHARP_SCIP_CONCURRENCY)` |
| Phase 3 | Batch reference cache pre-warm | `run_phase_3_prewarm()`, gated by `CSHARP_PREWARM_ENABLED` (default: true) |

### scip-csharp subcommands

| Subcommand | Purpose |
|---|---|
| `index` | Compile solution, emit definitions only (fast) |
| `find-refs` | Resolve references for ONE symbol on demand (lazy) |
| `batch-find-refs` | Resolve references for ALL symbols in one workspace session (Phase 3 pre-warm) |

### `SymbolIndexerRegistry` ownership

4 `Arc::new(SymbolIndexerRegistry::new())` sites: `IndexManager::new()`, `IndexManager::new_for_path()`, `ServeState::new()`, `CodesearchService::new_with_stores()`. `CodesearchService::new_for_serve()` clones from `ServeState`.

### `SymbolIndexer` trait

The trait includes `as_any()` for downcasting to concrete types (needed for Phase 3 pre-warm which calls `CSharpSymbolIndexer::prewarm_ref_cache()`).

## Known Bugs

### Bug 1 — `.gitignore` not respected by file watcher / vector indexer (HIGH)

Build artifacts (`obj/`, `bin/`, `.claude/`) indexed as source files. Fix: parse `.gitignore` via `ignore` crate (already a dependency) in FSW and vector indexer.

### Bug 2 — MSBuildWorkspace picks up `obj/` generated files as duplicate Compile items (HIGH)

Auto-generated files in `obj/Debug/` cause duplicate Compile items, failing project load and cascading to all dependents.

**Workaround:** Add `Directory.Build.props` at solution root with `<Compile Remove="obj\**" />`.
**Proper fix:** Pass `DesignTimeBuild=true` + `SkipCompilerExecution=true` in scip-csharp.

### Bug 3 — `--filter-project` selects wrong project when workspace fails to load (MEDIUM)

Changed `.cs` files in a failed project silently reassigned to a sibling project. Fix: log warning and skip reassignment.

## Bugs found during live testing (2026-05-08)

| ID | Severity | Title | Status |
|----|----------|-------|--------|
| B1 | 🔴 CRITICAL | ExampleRepo double-indexed (~47k chunks, expected ~24k) | Fixed by force reindex; root cause: index built twice without clear |
| B2 | 🔴 CRITICAL | `status(kind="projects")` reports 0 chunks for all repos | Open — projects aggregation reads chunk counts incorrectly |
| B3 | 🟡 MEDIUM | Regex `\w+`/`\b` returns empty in literal mode | Open — BM25 tokenizes before regex evaluation |
| B4 | 🟡 MEDIUM | `find_impact` returns duplicate definitions (with/without `src/` prefix) | Open — SCIP symbols indexed with two path representations |
| B5 | 🟠 LOW | JavaScript noise in `find definition` group-scope | Open — no language filter in group context |
| B6 | 🟠 LOW | BM25 prefix-matching doesn't work (`TestPlan` ≠ `TestPlanCache`) | Open — no subword tokenization |
| B7 | 🔴 HIGH | Apparent delete failure (consequence of B1 duplicate chunks) | Resolved by fixing B1 |

**Test summary (v1.0.93, 3 repos, 12k-24k chunks each):** Semantic search 5/5, literal exact-match ✅, find definition/usages ✅, explore outline ✅, find_impact ✅, multi-repo group search ✅, file watcher ✅, edge cases (unknown alias, missing symbol, wide regex) ✅. Performance: search <500ms, literal <1s, group <2s.

## Remaining work

- [ ] Verify on live large repo: 1st `find_impact` triggers lazy find-refs, 2nd+ call < 100ms (cache hit)
- [ ] CI green on `csharp-integration-tests` job *(first run after push)*
- [ ] Minor: warn if `--filter-project` passed to `find-refs` CLI (currently silently ignored)
- [ ] Minor: `FindRefsOutput.Symbol` should be `init` not `set` (consistency)
- [ ] Known limitation: first `find_impact` on un-cached symbol triggers full workspace open (2-5 min on large solution); Phase 3 pre-warm mitigates. Daemon mode (persistent workspace) out of scope.
- [ ] Standalone `index symbol` — local symbol index without serve running (currently requires HTTP API)

## Notes for OpenCode

- **Validation**: `cargo check` and `cargo clippy` for iteration. **No `--release` builds — always dev/debug.** Run `cargo test --lib` or `cargo test --bin` only when logic changes affect tests — otherwise it's wasted time.
- `scip-csharp` is self-contained single-file .NET 10 publish (no runtime required on target)
- `scip-csharp` is stateless, runs once per indexing request
- Roslyn may yield partial output on compilation failures — acceptable
- Symbol resolution: exact match first, then fuzzy via `scip_simple_names`
- Position lookup matches `start_line` only (not `[start_line, end_line]` range)

### ⚠️ LMDB Access Rule — CRITICAL

LMDB **does not allow** two `EnvOpenOptions::open()` handles on the same directory in the same process. Violating this causes runtime panics and corrupted indexes.

**In serve context (`codesearch serve`):** ALL LMDB access MUST go through `get_or_open_stores()` (serve/mod.rs) which returns `Arc<SharedStores>`. This is the single entry point that ensures one LMDB handle per `.codesearch.db`.

**Forbidden in serve/MCP code:**
- `VectorStore::new()` — opens its own LMDB environment
- `VectorStore::open_readonly()` — same issue
- Any direct `heed::EnvOpenOptions::open()` on a `.codesearch.db` path

**Allowed in CLI/stdio context:** `VectorStore::new()` is fine when codesearch runs as a standalone CLI tool (own process, no conflicting handles).

**The 4 LMDB environments in this codebase:**
1. Vector DB — `.codesearch.db/` via `VectorStore` (serve: through `SharedStores` only)
2. SCIP symbols — `.codesearch.db/scip/` via `open_scip_env()` (separate dir, separate handle, safe)
3. Embed cache — `~/.codesearch/embed_cache/` via `EmbeddingCache` (global path, separate dir, safe)
4. FTS — `.codesearch.db/fts/` — Tantivy, NOT LMDB (no constraint)

**If you add a new feature that needs LMDB in serve context:** Use `get_or_open_stores()` to get the shared handle. Never open a second handle on the same path.

### Runtime vs build locations

- **Runtime**: `C:\Users\develterf\.local\bin\` — contains `codesearch.exe` and `helpers/csharp/scip-csharp.exe`. This is where `codesearch serve` runs from.
- **Build**: `target/release/` — this folder lives **outside the repo** (set via `CARGO_TARGET_DIR`). For compilation only. Never run codesearch from this location.
- The helper detection uses `<codesearch-exe-dir>/helpers/csharp/scip-csharp.exe` — so the helper must live next to the codesearch binary at runtime.
- **Logs**: `~\.codesearch\logs\` — codesearch writes structured logs here during serve. Check these for startup errors, rebuild failures, and helper detection messages.

### Deploying to runtime

- `..\copy-to-common.ps1` — builds and copies **both** `codesearch.exe` and `scip-csharp.exe` to `~/.local/bin/` (the common execution dir). Use this to update the runtime binaries. **No `--release` builds — always dev/debug.**
- The helper is built via: `dotnet publish helpers/csharp/scip-csharp.csproj -r win-x64 --self-contained -c Release`
- Helper output must be **single-file only**: `scip-csharp.exe` (+ optional `.pdb`). The `.csproj` has `PublishSingleFile=true` which bundles everything into one exe.
- Do NOT copy framework DLLs, `BuildHost-*` dirs, or `.dll.config` files to the runtime location — only the single `.exe` is needed.
