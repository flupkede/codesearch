# AGENTS.md — features/symbol-references

## Goal

Add symbol-aware reference lookups to codesearch. New MCP tool `find_impact(symbol)` returns transitive call-sites of a symbol with file/line precision, enabling agents to plan refactors with IDE-class accuracy instead of relying on text-matching grep heuristics.

MVP scope is **C# only**. Architecture is language-agnostic: a per-language adapter behind a uniform `SymbolIndexer` trait so future branches can plug in Python (scip-python), TypeScript (scip-typescript), Rust (scip-rust), etc. without redesigning.

This branch addresses the most concrete feature gap with SocratiCode and Serena. Codesearch keeps its lightweight Rust-binary identity; semantic analysis for C# happens in a small, optional .NET helper bundled with the release.

---

## Architecture

### Per-language adapter pattern

```
src/symbols/
├── mod.rs            # SymbolIndexer trait + dispatch
├── csharp.rs         # C# adapter: locates helper, invokes subprocess, parses SCIP, stores in LMDB
└── scip_parse.rs     # thin wrapper around the `scip` crate (SCIP protobuf bindings)
```

The trait shape:

```rust
trait SymbolIndexer: Send + Sync {
    /// Run the indexer for this language over the repo. Writes results to LMDB.
    /// Idempotent: safe to re-run after file changes.
    async fn rebuild(&self, repo: &Repo, scope: RebuildScope) -> Result<RebuildSummary>;

    /// Return the symbol's references from the LMDB store.
    fn find_references(&self, repo: &Repo, symbol: &CanonicalSymbol) -> Result<Vec<Reference>>;
}

enum RebuildScope {
    Full,                   // entire solution / project tree
    Project(PathBuf),       // single .csproj or equivalent
    Files(Vec<PathBuf>),    // future: per-file (out of MVP scope)
}
```

Future languages register additional `SymbolIndexer` impls in `src/symbols/mod.rs`. For MVP only `csharp.rs` exists.

### C# helper

`helpers/csharp/` is a small C# project that wraps Roslyn's `SymbolFinder.FindReferencesAsync()` and writes a SCIP protobuf file. It is invoked by codesearch as a subprocess on a debounced rebuild trigger.

CLI shape:

```
scip-csharp index --solution path\to\X.sln --output path\to\index.scip [--project path\to\Y.csproj]
```

Behavior:
1. `MSBuildLocator.RegisterDefaults()` to discover MSBuild
2. `MSBuildWorkspace.Create()` and load solution (or single project if `--project` given)
3. Walk symbols of interest (methods, properties, classes, interfaces, fields)
4. For each, `SymbolFinder.FindReferencesAsync(symbol, workspace.CurrentSolution)`
5. Serialize to SCIP protobuf (sourcegraph/scip schema)
6. Write to `--output`

Error handling: compilation errors anywhere in the solution must NOT abort the indexer. Roslyn will produce partial semantic info for what does compile — emit those references, log warnings for the rest, exit code 0. Codesearch reads the partial index; gaps are acceptable, crashes are not.

### LMDB storage

New LMDB table per repo: `scip_symbols`. Keyed by canonical SCIP symbol string (`csharp . . . FieldDefinition#Validate().`), value is a serialized list of `(file_path, start_line, end_line, kind)` tuples.

Symbol resolution at query time: agent passes a name like `FieldDefinition.Validate` or a `file:line` position. Adapter resolves to canonical SCIP symbol, looks up references in LMDB, returns the list.

### Rebuild trigger

The existing file watcher (`src/watch/`) fires on file changes. Add a debounced trigger for `.cs` file changes: 60-second quiet period, then call the C# helper with `RebuildScope::Project` (find the `.csproj` containing the changed file) or `RebuildScope::Full` (multiple projects affected, or first run).

The rebuild runs in a background thread. BM25 and vector search are unaffected. `find_impact` queries hitting an in-progress rebuild return the existing (older) index with `index_age_seconds` exposed in the response so the agent can reason about staleness if needed.

### MCP tool: `find_impact`

New tool exposed via `src/mcp/`. Input variants:

```json
{ "symbol_name": "FieldDefinition.Validate", "project": "boin-aprimo" }
{ "file": "src/Validation/FieldDefinition.cs", "line": 42, "project": "boin-aprimo" }
```

Response:

```json
{
  "symbol": "csharp . . . FieldDefinition#Validate().",
  "references": [
    { "file": "src/Importer/RecordImporter.cs", "start_line": 87, "end_line": 87, "kind": "call" },
    { "file": "src/Tests/ValidationTests.cs", "start_line": 23, "end_line": 23, "kind": "call" },
    ...
  ],
  "index_age_seconds": 12,
  "language": "csharp",
  "scope": "project:boin-aprimo"
}
```

Errors: if the helper isn't installed, the symbol index is missing, or the language isn't supported, return a structured error with `available_languages` and `hint_for_agent` similar to the existing `scope_required` pattern in multi-repo serve.

---

## Files to create

### Helper (C#)

| File | Purpose |
|------|---------|
| `helpers/csharp/scip-csharp.csproj` | net10.0, references Microsoft.CodeAnalysis (>= 4.13), Microsoft.Build.Locator, Google.Protobuf |
| `helpers/csharp/Program.cs` | CLI entrypoint; argument parsing |
| `helpers/csharp/SymbolIndexer.cs` | Roslyn workspace + SymbolFinder logic |
| `helpers/csharp/ScipWriter.cs` | SCIP protobuf serialization |
| `helpers/csharp/Scip.proto` | Vendored from sourcegraph/scip (or referenced via NuGet if available) |
| `helpers/csharp/README.md` | Internal dev docs only — explains how to build/test the helper for contributors. NOT user-facing. |
| `helpers/csharp/tests/Fixtures/SmallSolution/` | Minimal C# solution used for snapshot tests |
| `helpers/csharp/tests/IndexerTests.cs` | Snapshot tests using the fixture |

### Rust

| File | Purpose |
|------|---------|
| `src/symbols/mod.rs` | `SymbolIndexer` trait, language dispatch, common types |
| `src/symbols/csharp.rs` | C# adapter (helper detection, subprocess invocation, SCIP parsing → LMDB) |
| `src/symbols/scip_parse.rs` | Thin wrapper around the `scip` crate |
| `tests/symbols_csharp_test.rs` | Integration test: small fixture solution → assert references |

---

## Files to modify

| File | Change |
|------|--------|
| `Cargo.toml` | Add `scip = "..."` dependency (Sourcegraph's Rust SCIP bindings). Pin to a known-stable version. |
| `src/lib.rs` | `pub mod symbols;` |
| `src/mcp/mod.rs` (or wherever tool registration lives) | Register `find_impact` |
| `src/serve/mod.rs` | Wire `find_impact` through MCP service layer |
| `src/watch/mod.rs` | Add `.cs` change handler with 60s debounce → triggers `csharp::rebuild` |
| `src/cli/mod.rs` | Add `codesearch reindex <alias> --symbols` flag for forced symbol-index rebuild |
| `.github/workflows/release.yml` | Extend matrix to produce `-with-csharp` variants per platform (see "CI / Release changes" below) |
| `README.md` | New top-level section: "C# semantic search" — explains the two release variants and what to download |
| `CHANGELOG.md` | Entry for `find_impact` MCP tool, C# helper, new release variants |

---

## CI / Release changes

Existing release workflow produces 3 archives (windows zip, linux tar.gz, macos tar.gz). After this branch: **6 archives** — the existing 3 (kale, unchanged) plus 3 `-with-csharp` variants per platform.

Naming:
```
codesearch-windows-x86_64.zip                   (existing, unchanged)
codesearch-windows-x86_64-with-csharp.zip       (new)
codesearch-linux-x86_64.tar.gz                  (existing, unchanged)
codesearch-linux-x86_64-with-csharp.tar.gz      (new)
codesearch-macos-arm64.tar.gz                   (existing, unchanged)
codesearch-macos-arm64-with-csharp.tar.gz       (new)
```

Per-platform job adds:
1. `actions/setup-dotnet@v4` with `dotnet-version: '10.0.x'`
2. `dotnet publish helpers/csharp -c Release -r {rid} --no-self-contained -o helpers-publish`
3. Stage Rust binary + `helpers-publish/*` into `helpers/csharp/` subdirectory
4. Pack as `-with-csharp` archive next to the existing kale archive

`--no-self-contained` is intentional: the helper is framework-dependent and small (~5-15 MB total), relying on the user's installed .NET 10 runtime. Self-contained would add 60-80 MB per platform for no real-world benefit, since C# users have .NET 10 anyway.

The macOS-arm64 job remains opt-in via `include_macos: false` workflow input. When skipped, only the kale macOS archive is missing — same behavior as today.

---

## Helper detection at runtime

Codesearch looks for the helper in this order:

1. `<codesearch-exe-dir>/helpers/csharp/scip-csharp[.exe]` (when running from a `-with-csharp` release archive)
2. `$PATH` lookup of `scip-csharp` (when user installed via `dotnet tool install --global` or similar)
3. Configurable override via `CODESEARCH_SCIP_CSHARP` environment variable

If none found and a C# repo is registered: log one clear message at warn level, leave `find_impact` unavailable for that repo's C# language, but do NOT fail the overall indexing. BM25/vector search continues to work.

---

## Quality gates

- [ ] `cargo check` clean
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` clean
- [ ] `cargo test --lib --bins` all pass
- [ ] `dotnet test helpers/csharp/` all pass (snapshot tests on the small fixture solution)
- [ ] CI release workflow produces all 6 archives on a tag push
- [ ] Manual: extract `codesearch-windows-x86_64-with-csharp.zip`, register a real Aprimo client repo, run `find_impact` for a known method, verify the reference list matches Visual Studio's "Find All References"
- [ ] Manual: edit a `.cs` file, wait > 60s, observe SCIP rebuild triggered in logs, query `find_impact`, verify the new reference appears
- [ ] Manual: extract the kale variant (no helper), register a C# repo, verify a clean warning message and that BM25/vector search still works

---

## Out of scope

- Languages other than C#. Trait is in place; future branches add adapters.
- LSP-based live mode. SCIP-only is the deliberate choice (planning discussion: IDE already runs Roslyn; second LSP is wasteful).
- Stack Graphs, Kythe, or Glean integration.
- Interactive HTML graph viewer. Decided against — no measurable user value.
- Per-symbol incremental SCIP merge. Per-project is the smallest practical rebuild unit.
- Native AOT compile of the helper. Roslyn AOT is unstable; framework-dependent build is the right MVP choice.
- Cross-language reference graphs (e.g. Python service calling C# REST endpoint). Each language's adapter is independent for now.

---

## Branch flow

```powershell
# already on features/symbol-references (branched from develop)
# implement, test, commit incrementally
git push origin features/symbol-references

# When done: PR features/symbol-references → develop
```

---

## Done when

- [ ] C# helper builds and produces valid SCIP output on the fixture solution
- [ ] Codesearch detects the helper, invokes it on register/rebuild, parses SCIP, stores references in LMDB
- [ ] `find_impact` MCP tool returns correct references for known C# symbols on a real repo
- [ ] File watcher triggers debounced helper rebuilds on `.cs` changes
- [ ] CI produces all 6 release archives
- [ ] README has a "C# semantic search" section explaining the release variants
- [ ] CHANGELOG entry written
- [ ] Manual test passes on a real Aprimo client repo: query → SCIP-derived references → matches IDE behavior
- [ ] PR opened against `develop`

---

## Notes for OpenCode

This is a multi-language change (Rust + C#). Two build systems coexist in this repo. `cargo` and `dotnet` do not interfere.

Key library choices, pinned for stability:
- Rust side: the [`scip` crate](https://crates.io/crates/scip) from Sourcegraph for protobuf decoding. It is the canonical Rust binding for the SCIP schema.
- C# side: `Microsoft.CodeAnalysis` >= 4.13 (covers C# 14 / .NET 10 syntax). `Microsoft.Build.Locator` is required to register MSBuild before loading any workspace, otherwise `MSBuildWorkspace.Create()` throws.
- `Google.Protobuf` for SCIP protobuf serialization on the C# side.

Roslyn compilation requires the target solution to be buildable. If `dotnet build` on the user's solution fails, the helper produces partial output for the parts that do compile. Document this in the helper's README and in error messages — partial output is expected behavior, not a bug.

The `scip-csharp` CLI is stateless: each invocation does one indexing run and exits. No daemon, no IPC. Codesearch invokes it as a subprocess. This keeps the helper simple and easy to reason about.

Symbol resolution from a name like `FieldDefinition.Validate` to a canonical SCIP symbol string requires walking the SCIP index's `external_symbols` and `documents.occurrences` tables. Implement this lookup pragmatically — exact-match first, then fuzzy-match if no exact hit. Document the heuristic clearly in `symbols/csharp.rs`.

When in doubt about scope, prefer simpler. This branch is meant to ship; gold-plating any of the sub-systems extends it past usefulness.
