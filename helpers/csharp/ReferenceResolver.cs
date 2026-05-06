using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.FindSymbols;

namespace ScipCsharp;

/// <summary>
/// Resolves references for SCIP symbols on demand.
///
/// Used by the `find-refs` and `batch-find-refs` subcommands: the workspace is already
/// loaded (same startup cost as `index`), but instead of calling FindReferencesAsync for
/// every symbol in the solution, we call it only for the requested symbol(s).
///
/// The Rust host caches results in LMDB so subsequent find_impact calls for the
/// same symbol are instant (O(1) LMDB read).
/// </summary>
public sealed class ReferenceResolver
{
    /// <summary>
    /// Resolve all references for a single SCIP symbol key within the solution.
    /// </summary>
    public async Task<FindRefsOutput> FindRefsAsync(Solution solution, string scipKey)
    {
        var output = new FindRefsOutput { Symbol = scipKey };

        var (symbolMap, projectRoot) = await BuildSymbolMapAsync(solution).ConfigureAwait(false);

        var targetSymbol = FindSymbolByKey(symbolMap, scipKey);
        if (targetSymbol is null)
        {
            Console.Error.WriteLine($"[WARN] find-refs: symbol not found in map: {scipKey}");
            return output;
        }

        Console.Error.WriteLine($"find-refs: resolving references for {scipKey}...");

        var refs = await ResolveReferencesAsync(targetSymbol, solution, projectRoot).ConfigureAwait(false);
        output.References.AddRange(refs);

        Console.Error.WriteLine($"find-refs: found {output.References.Count} reference(s)");
        return output;
    }

    /// <summary>
    /// Resolve references for multiple SCIP symbol keys in a single workspace session.
    /// Builds the symbol map once, then iterates through all requested symbols.
    /// </summary>
    public async Task<BatchFindRefsOutput> BatchFindRefsAsync(Solution solution, IReadOnlyList<string> scipKeys)
    {
        var results = new BatchFindRefsOutput();

        var (symbolMap, projectRoot) = await BuildSymbolMapAsync(solution).ConfigureAwait(false);

        // Build reverse map: scip_key → ISymbol for O(1) lookup
        var keyToSymbol = new Dictionary<string, ISymbol>();
        foreach (var kv in symbolMap)
        {
            keyToSymbol[kv.Value] = kv.Key;
        }

        Console.Error.WriteLine($"batch-find-refs: resolving references for {scipKeys.Count} symbol(s)...");

        for (int i = 0; i < scipKeys.Count; i++)
        {
            var scipKey = scipKeys[i];
            var output = new FindRefsOutput { Symbol = scipKey };

            if (!keyToSymbol.TryGetValue(scipKey, out var targetSymbol))
            {
                Console.Error.WriteLine($"batch-find-refs: [{i + 1}/{scipKeys.Count}] symbol not found: {scipKey}");
                results.Results.Add(output);
                continue;
            }

            var refs = await ResolveReferencesAsync(targetSymbol, solution, projectRoot).ConfigureAwait(false);
            output.References.AddRange(refs);

            Console.Error.WriteLine($"batch-find-refs: [{i + 1}/{scipKeys.Count}] {scipKey} → {refs.Count} ref(s)");
            results.Results.Add(output);
        }

        var totalRefs = results.Results.Sum(r => r.References.Count);
        Console.Error.WriteLine($"batch-find-refs: complete — {scipKeys.Count} symbols, {totalRefs} total references");
        return results;
    }

    /// <summary>
    /// Builds the symbol map by compiling all projects in the solution.
    /// Returns the map and the common project root for relative path computation.
    /// </summary>
    private async Task<(Dictionary<ISymbol, string> SymbolMap, string? ProjectRoot)> BuildSymbolMapAsync(Solution solution)
    {
        var symbolMap = new Dictionary<ISymbol, string>(SymbolEqualityComparer.Default);

        Console.Error.WriteLine("find-refs: building symbol map from solution...");
        foreach (var project in solution.Projects)
        {
            var compilation = await project.GetCompilationAsync().ConfigureAwait(false);
            if (compilation is null)
            {
                Console.Error.WriteLine($"[WARN] find-refs: could not compile {project.Name}");
                continue;
            }
            SymbolIndexer.CollectSymbols(compilation.GlobalNamespace, symbolMap);
        }
        Console.Error.WriteLine($"find-refs: symbol map built ({symbolMap.Count} symbols)");

        var projectRoot = SymbolIndexer.FindCommonRoot(
            solution.Projects
                .Select(p => p.FilePath)
                .Where(p => p is not null)
                .Cast<string>());

        return (symbolMap, projectRoot);
    }

    private static ISymbol? FindSymbolByKey(Dictionary<ISymbol, string> symbolMap, string scipKey)
    {
        foreach (var kv in symbolMap)
        {
            if (kv.Value == scipKey)
                return kv.Key;
        }
        return null;
    }

    private static async Task<List<FindRefsOccurrence>> ResolveReferencesAsync(
        ISymbol targetSymbol, Solution solution, string? projectRoot)
    {
        var results = new List<FindRefsOccurrence>();

        try
        {
            var refResults = await SymbolFinder
                .FindReferencesAsync(targetSymbol, solution)
                .ConfigureAwait(false);

            foreach (var refLocation in refResults.SelectMany(r => r.Locations))
            {
                var loc = refLocation.Location;
                if (!loc.IsInSource) continue;

                var relPath = SymbolIndexer.MakeRelative(loc.SourceTree?.FilePath, projectRoot);
                if (relPath is null) continue;

                var lineSpan = loc.GetLineSpan();
                results.Add(new FindRefsOccurrence
                {
                    File = relPath,
                    StartLine = lineSpan.StartLinePosition.Line + 1,
                    EndLine = lineSpan.EndLinePosition.Line + 1,
                    Kind = "reference",
                });
            }
        }
        catch (Exception ex)
        {
            Console.Error.WriteLine(
                $"[WARN] FindReferencesAsync failed for {targetSymbol.Name}: " +
                $"{ex.GetType().Name}: {ex.Message}");
        }

        return results;
    }
}
