# CLAUDE.md — codesearch (features/symbol-references)

## ‼️ IMPORTANT — READ AGENTS.md FIRST, BEFORE STARTING ANY WORK ‼️

Before answering, planning, or running any tool: **open and read `AGENTS.md` at the repo root**. It contains the authoritative project goal, architecture, runtime layout (where `codesearch.exe` and `helpers/csharp/scip-csharp.exe` actually live), log locations, publish recipe, and current status. This file (.claude/CLAUDE.md) is supplemental — AGENTS.md is the source of truth.

## Goal

Add symbol-aware reference lookups to codesearch via `find_impact` MCP tool. Returns file/line-precise references so agents can plan refactors with IDE-level accuracy. MVP is **C# only**; architecture is language-agnostic through per-language `SymbolIndexer` adapters.

## Implemented Features

- **`find_impact` MCP tool** — returns transitive call-sites for a symbol (name-based or position-based), C# via `scip-csharp` helper
- **`scip-csharp` helper** — .NET 10 CLI wrapping Roslyn `SymbolFinder.FindReferencesAsync()`, runs as subprocess
- **O(1) position lookup** — `scip_positions` LMDB table maps `(file:line)` → `[symbol_keys]`
- **O(1) fuzzy lookup** — `scip_simple_names` LMDB table maps last-segment identifier → `[full_keys]`
- **Bincode schema versioning** — version byte prefix on all LMDB payloads, clear error on mismatch
- **JSON version validation** — rejects scip-csharp index versions other than `"1.0"`
- **Helper failure cache** — `detect_helper()` caches both found and not-found results (`Mutex<Option<Option<PathBuf>>>`)
- **Shared `SymbolIndexerRegistry`** — `ServeState`, `CodesearchService`, and `IndexManager` each own one `Arc<Registry>`; no per-request instantiation
- **`.cs` watcher debounce** — 60s quiet period triggers automatic symbol rebuild
- **`-with-csharp` release variants** — 6 release archives (3 plain + 3 with helper)
- **Gated integration test** — `csharp_helper_integration` cargo feature for full-pipeline testing
- **CI** — separate `csharp-integration-tests` job in `.github/workflows/ci.yml`
- **Stale-path resilience + derived alias** — moved/renamed indexed folders no longer crash serve: `git_remote` captured at registration, `reconcile_all_paths()` best-effort relocates by matching `remote.origin.url` (bounded depth, `CODESEARCH_RELOCATE_MAX_DEPTH`, default 3) else warn+skip; `codesearch index prune` for manual cleanup. The `--alias` flag was removed (alias always = directory name). `ReposConfig::reconcile()` hardens hand-edited `repos.json` on load. See AGENTS.md for details.

## Architecture

### Per-language adapter pattern

`src/symbols/` hosts the adapter layer:

- `mod.rs` — `SymbolIndexer` trait + `SymbolIndexerRegistry` dispatch
- `csharp.rs` — C# adapter (rebuild, find_references, find_references_by_position)
- `scip_parse.rs` — JSON parser for scip-csharp output

### LMDB tables

| Table | Key | Value |
|---|---|---|
| `scip_symbols` | full SCIP key | `[v1, bincode(Vec<StoredReference>)]` |
| `scip_positions` | `<file>:<line>` (forward-slash) | `[v1, bincode(Vec<String>)]` |
| `scip_simple_names` | last segment of canonical symbol | `[v1, bincode(Vec<String>)]` |
| `scip_meta` | `last_rebuild_ts`, `symbol_count` | `Str` |

### MCP tool: `find_impact`

Inputs:
- `{ "symbol_name": "FieldDefinition.Validate", "project": "alias" }`
- `{ "file": "src/X.cs", "line": 42, "project": "alias" }`

Returns references with `file`, `start_line`, `end_line`, `kind` + `index_age_seconds`.

### Helper detection lookup order

1. `CODESEARCH_SCIP_CSHARP` env var
2. `<codesearch-exe-dir>/helpers/csharp/scip-csharp[.exe]`
3. `$PATH`

Missing helper disables `find_impact` for C# only — all other features keep working.

### `SymbolIndexerRegistry` ownership

Exactly 4 `Arc::new(SymbolIndexerRegistry::new())` sites:
1. `IndexManager::new()` (watcher path)
2. `IndexManager::new_for_path()` (direct path)
3. `ServeState::new()` (serve HTTP path)
4. `CodesearchService::new_with_stores()` (standalone MCP/stdio path)

`CodesearchService::new_for_serve()` clones from `ServeState`.

## Remaining post-merge work

- [ ] CI green on GitHub Actions for `csharp-integration-tests` job *(first run after push)*
- [ ] `REVIEW_features-symbol-references.md` closing section "Fixes applied" with commit SHAs
- [ ] Manual end-test on real client repo: 2nd/3rd `find_impact` call < 100ms
- [ ] 49 minors from review — separate in next iteration
- [ ] Follow-up majors: rebuild scope, sequential text+symbol rebuild, git rev-parse subprocess, per-repo mutex for parallel rebuilds
- [x] LMDB `map_size` — **fixed**: SCIP LMDB raised from 64 MB → 512 MB (virtual); env-var override `CODESEARCH_SCIP_LMDB_MAP_MB`

## CI / Release

Release output: 6 archives (3 plain + 3 `-with-csharp`). Each `-with-csharp` job installs .NET 10, publishes `helpers/csharp`, stages helper alongside binary.

Quality gates: `cargo check`, `cargo clippy`, `cargo test --lib --bins`, `dotnet test helpers/csharp/`, CI green.

## Tooling rules (IMPORTANT)

- **Do NOT use the `codesearch` CLI/exe to investigate this repo.** Codesearch is the project under development and is currently potentially broken — using our own broken tool to debug itself is unreliable.
- **Codesearch must always be used via its MCP server tools** (when available), never via the bundled binary at the shell.
- **For this repo, fall back to `grep` / `Glob` / `Read`** for all discovery and navigation until codesearch is verified working again.

## Notes

- Rust + C# codebase; `cargo` and `dotnet` do not interfere
- `scip-csharp` is stateless, runs once per indexing request
- Roslyn may yield partial output on compilation failures — acceptable
- Symbol resolution: exact match first, then fuzzy via `scip_simple_names`
- Position lookup matches `start_line` only (not `[start_line, end_line]` range)
- `copy-to-common.ps1` builds helper via `dotnet publish` and copies to `~/.local/bin/helpers/csharp/`
