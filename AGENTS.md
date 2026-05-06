# AGENTS.md — features/symbol-references

## Goal

Add symbol-aware reference lookups to codesearch via `find_impact` MCP tool. Returns file/line-precise references so agents can plan refactors with IDE-level accuracy. MVP is **C# only**; architecture is language-agnostic through per-language `SymbolIndexer` adapters.

## Implemented Features

- **`find_impact` MCP tool** — returns transitive call-sites for a symbol (name-based or position-based), C# via `scip-csharp` helper
- **`scip-csharp` helper** — .NET 10 CLI wrapping Roslyn. **Two subcommands**:
  - `index` — compile solution, emit **definitions only** (no FindReferencesAsync at rebuild time = 10–50× faster)
  - `find-refs --symbol <key>` — resolve references for ONE symbol on demand (lazy, result cached in `scip_ref_cache`)
- **Opt 1 — external-type filter** — `CollectTypeSymbols` skips all types with no `IsInSource` location (framework/NuGet), 10-100× fewer symbols on large solutions
- **Opt 2 — lazy reference resolution** — rebuild stores definitions only; `find_references()` checks `scip_ref_cache` first, calls `scip-csharp find-refs` on cache miss, then caches result; `block_in_place` in MCP handler for blocking subprocess
- **Opt 3 — incremental merge** — `RebuildScope::Files`: uses position index as reverse map to collect stale symbol keys, merges new definitions (partial-class safe: keeps defs from non-affected files), rebuilds `simple_names` from all current symbols
- **O(1) position lookup** — `scip_positions` LMDB table maps `(file:line)` → `[symbol_keys]`
- **O(1) fuzzy lookup** — `scip_simple_names` LMDB table maps last-segment identifier → `[full_keys]`
- **`scip_ref_cache` LMDB table** — key: SCIP symbol key; value: bincode(Vec<StoredReference>); populated on first `find_impact` per symbol, cleared on any rebuild
- **Bincode schema versioning** — version byte prefix on all LMDB payloads, clear error on mismatch
- **JSON version validation** — rejects scip-csharp index versions other than `"1.0"`
- **Backward compat** — old LMDB indexes (pre-Opt2, with references in `scip_symbols`) still work; `has_legacy_refs` check bypasses lazy invocation
- **Helper failure cache** — `detect_helper()` caches both found and not-found results (`Mutex<Option<Option<PathBuf>>>`)
- **Shared `SymbolIndexerRegistry`** — `ServeState`, `CodesearchService`, and `IndexManager` each own one `Arc<Registry>`; no per-request instantiation
- **`.cs` watcher debounce** — 60s quiet period triggers automatic symbol rebuild
- **`-with-csharp` release variants** — 6 release archives (3 plain + 3 with self-contained helper)
- **Gated integration test** — `csharp_helper_integration` cargo feature for full-pipeline testing
- **CI** — separate `csharp-integration-tests` job in `.github/workflows/ci.yml`
- **Sequential phase-2 startup** — Phase 1 warms repos sequentially, Phase 2 runs gated C# SCIP rebuilds ordered by `last_changed_unix` under `Semaphore(concurrency)` via `CSHARP_SCIP_CONCURRENCY` env (default **2**, clamp [1,4])
- **`repos_meta` tracking** — `RepoMeta` (last_changed_unix, last_scip_indexed_unix) persisted in `repos.json` with debounced save (10s window)
- **TUI C# indicator** — in status column: green `C#·` ready, yellow `C#…` indexing, red `C#!` error; footer shows helper availability; Calls column with tool call count
- **Selective ref cache invalidation** — incremental rebuilds only purge cached refs for affected symbols, not entire cache
- **Phase 3 pre-warm** — after Phase 2 definitions, `scip-csharp batch-find-refs` resolves all uncached symbols in a single workspace session; controlled by `CSHARP_PREWARM_ENABLED` env (default: true)
- **`index symbol` CLI** — `codesearch index symbol [-f] <alias>` for symbol-only rebuild; `--symbols` flag on `index -f` for combined text+symbol rebuild
- **Watcher .csproj grouping** — changed .cs files grouped by .csproj, incremental rebuild per project instead of full solution

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

## Current commit state (2026-05-06)

Latest commits on `features/symbol-references`:
- `35bbf36` fix: review remarks (double-Env, partial-class merge, META_SYMBOL_COUNT, temp collision)
- `bb8c1c8` feat: Opt1+2+3 — filter external types, lazy refs, incremental merge
- `6fc7861` feat: live progress streaming from scip-csharp (stage 6)
- `88a8f01` fix: ordering + concurrency default=2 (stage 5)
- `becc518` fix: IncludeAllContentForSelfExtract + MSBuild registration (stage 4)
- `4ed0a3f` fix: applies_to + non-C# repos red TUI (stage 3)

**Status**: `cargo check` + `cargo clippy` clean, `dotnet build` clean.
**Deployed**: Run `..\copy-to-common.ps1` to deploy to `~/.local/bin/`.

## Remaining work

- [ ] Verify on live Aprimo repo: 1st `find_impact` call triggers lazy find-refs, 2nd+ call < 100ms (cache hit)
- [ ] CI green on `csharp-integration-tests` job *(first run after push)*
- [ ] Minor: warn if `--filter-project` passed to `find-refs` CLI (currently silently ignored)
- [ ] Minor: `FindRefsOutput.Symbol` should be `init` not `set` (consistency)
- [ ] Known limitation: first `find_impact` on un-cached symbol triggers full workspace open (2-5 min on large solution); Phase 3 pre-warm mitigates this by batch-resolving all symbols at startup. Daemon mode (persistent workspace) would fully eliminate it but is out of scope.
- [ ] Standalone `index symbol` — local symbol index without serve running (currently requires HTTP API)

## Notes for OpenCode

- **Validation**: `cargo check` and `cargo clippy` for iteration. **No `--release` builds — always dev/debug.** Run `cargo test --lib` or `cargo test --bin` only when logic changes affect tests — otherwise it's wasted time.
- `scip-csharp` is self-contained single-file .NET 10 publish (no runtime required on target)
- `scip-csharp` is stateless, runs once per indexing request
- Roslyn may yield partial output on compilation failures — acceptable
- Symbol resolution: exact match first, then fuzzy via `scip_simple_names`
- Position lookup matches `start_line` only (not `[start_line, end_line]` range)

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
