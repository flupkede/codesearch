# C# Semantic Search

codesearch can expose IDE-like "Find All References" for C# through the optional `scip-csharp` helper.
It powers the `find_impact` MCP tool and is only relevant when you index C# repositories.

## Goal

- Find transitive references for C# symbols with file/line precision
- Keep normal search working even when the helper is missing
- Support both bundled and externally installed helper setups

## How it works

1. codesearch detects a C# repo through `.sln` or `.csproj` files
2. It launches `scip-csharp`
3. The helper uses Roslyn to build a SCIP symbol reference index
4. References are stored in LMDB under `scip_symbols`
5. `find_impact` reads that index and returns references

## Installation and setup

### Option 1: bundled release

Use one of the `-with-csharp` release archives. These include `scip-csharp` next to the codesearch binary.

### Option 2: external helper

Install the helper separately and point codesearch to it with:

```bash
CODESEARCH_SCIP_CSHARP=/path/to/scip-csharp
```

### Required runtime

- .NET 10 runtime
- A C# repo with a valid solution or project file

## Release packages

There are 6 release archives:

| Platform | codesearch | codesearch + C# |
|---|---|---|
| Windows x86_64 | `codesearch-windows-x86_64.zip` | `codesearch-windows-x86_64-with-csharp.zip` |
| Linux x86_64 | `codesearch-linux-x86_64.tar.gz` | `codesearch-linux-x86_64-with-csharp.tar.gz` |
| macOS ARM64 | `codesearch-macos-arm64.tar.gz` | `codesearch-macos-arm64-with-csharp.tar.gz` |

If you do not need C# symbol references, use the plain package.

## Helper lookup order

codesearch resolves `scip-csharp` in this order:

1. `CODESEARCH_SCIP_CSHARP`
2. `<codesearch-exe-dir>/helpers/csharp/scip-csharp[.exe]`
3. `$PATH`

## Testing

### Helper test

Run the helper test suite:

```bash
dotnet test helpers/csharp/
```

### Rust integration tests

Run the codesearch test suite:

```bash
cargo test --lib --bins
```

### End-to-end manual test

1. Start codesearch on a real C# repo
2. Ensure the helper is found
3. Reindex with symbols enabled
4. Query `find_impact` for a known method
5. Compare results with Visual Studio "Find All References"

### Watcher test

1. Change a `.cs` file
2. Wait longer than 60 seconds
3. Confirm a symbol rebuild is logged
4. Query `find_impact` again
5. Verify the new reference appears

## Notes

- Missing helper only disables `find_impact` for C#
- Search, `find`, `explore`, `get_chunk`, and `status` keep working
- Compilation errors in the target solution should not abort indexing completely
