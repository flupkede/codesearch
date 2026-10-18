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

        var (symbolMap, projectRoot, mapWarnings) = await BuildSymbolMapAsync(solution).ConfigureAwait(false);
        output.Warnings.AddRange(mapWarnings);

        var targetSymbol = FindSymbolByKey(symbolMap, scipKey);
        if (targetSymbol is null)
        {
            Console.Error.WriteLine($"[WARN] find-refs: symbol not found in map: {scipKey}");
            return output;
        }

        Console.Error.WriteLine($"find-refs: resolving references for {scipKey}...");

        var (refs, warnings) = await ResolveReferencesAsync(targetSymbol, solution, projectRoot).ConfigureAwait(false);
        output.References.AddRange(refs);
        output.Warnings.AddRange(warnings);

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

        var (symbolMap, projectRoot, mapWarnings) = await BuildSymbolMapAsync(solution).ConfigureAwait(false);

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
            // Map-level warnings apply to every symbol: a project that failed
            // to compile hides its symbols from every resolution.
            output.Warnings.AddRange(mapWarnings);

            if (!keyToSymbol.TryGetValue(scipKey, out var targetSymbol))
            {
                Console.Error.WriteLine($"batch-find-refs: [{i + 1}/{scipKeys.Count}] symbol not found: {scipKey}");
                results.Results.Add(output);
                continue;
            }

            var (refs, warnings) = await ResolveReferencesAsync(targetSymbol, solution, projectRoot).ConfigureAwait(false);
            output.References.AddRange(refs);
            output.Warnings.AddRange(warnings);

            Console.Error.WriteLine($"batch-find-refs: [{i + 1}/{scipKeys.Count}] {scipKey} → {refs.Count} ref(s)");
            results.Results.Add(output);
        }

        var totalRefs = results.Results.Sum(r => r.References.Count);
        Console.Error.WriteLine($"batch-find-refs: complete — {scipKeys.Count} symbols, {totalRefs} total references");
        return results;
    }

    /// <summary>
    /// Builds the symbol map by compiling all projects in the solution.
    /// Returns the map, the common project root for relative path computation,
    /// and one warning per project that failed to compile — those projects'
    /// symbols are silently missing from the map, so the caller must be told.
    /// </summary>
    private async Task<(Dictionary<ISymbol, string> SymbolMap, string? ProjectRoot, List<string> Warnings)> BuildSymbolMapAsync(Solution solution)
    {
        var symbolMap = new Dictionary<ISymbol, string>(SymbolEqualityComparer.Default);
        var warnings = new List<string>();

        Console.Error.WriteLine("find-refs: building symbol map from solution...");
        foreach (var project in solution.Projects)
        {
            // Same generator run as indexing: without it, Razor-generated
            // component types are missing from the map and every reference
            // inside them resolves to nothing.
            var (compilation, _) = await GeneratorSupport.GetCompilationWithGeneratorsAsync(project).ConfigureAwait(false);
            if (compilation is null)
            {
                Console.Error.WriteLine($"[WARN] find-refs: could not compile {project.Name}");
                warnings.Add($"could not compile project '{project.Name}' — its symbols are missing from the map");
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

        return (symbolMap, projectRoot, warnings);
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

    /// <summary>
    /// Picks the workspace counterpart of <paramref name="target"/> among the
    /// similar-symbol candidates: the exact signature match wins (the first
    /// candidate may be another overload), a lone candidate is accepted, and
    /// anything else returns <paramref name="target"/> with
    /// <paramref name="fellBack"/> set.
    /// </summary>
    internal static ISymbol PickWorkspaceSymbol(ISymbol target, IEnumerable<ISymbol> similar, out bool fellBack)
    {
        var candidates = similar.ToList();
        var signature = target.ToDisplayString(SymbolDisplayFormat.CSharpErrorMessageFormat);

        var exact = candidates.FirstOrDefault(c =>
            c.ToDisplayString(SymbolDisplayFormat.CSharpErrorMessageFormat) == signature);
        if (exact is not null)
        {
            fellBack = false;
            return exact;
        }

        if (candidates.Count == 1)
        {
            fellBack = false;
            return candidates[0];
        }

        fellBack = true;
        return target;
    }

    /// <summary>
    /// Resolves references for one symbol. Returns the occurrences plus a
    /// warning per survived failure — a caught FindReferencesAsync exception
    /// must degrade the answer's honesty, not just its size.
    /// </summary>
    private static async Task<(List<FindRefsOccurrence> Refs, List<string> Warnings)> ResolveReferencesAsync(
        ISymbol targetSymbol, Solution solution, string? projectRoot)
    {
        var results = new List<FindRefsOccurrence>();
        var warnings = new List<string>();

        try
        {
            // The symbol map is built from detached generator-augmented
            // compilations. For Razor component types SymbolFinder cannot
            // pair such a foreign symbol with its workspace counterpart and
            // returns instantly with nothing (observed live: a code-behind
            // method of a Blazor component answered 0 references in ~3ms
            // while the calls were visible to text search). Re-resolve the
            // symbol inside the containing project's workspace compilation
            // first; fall back to the foreign symbol when no counterpart
            // exists. Multi-target projects share an AssemblyName, so every
            // matching project is tried until one yields a counterpart.
            var workspaceSymbol = targetSymbol;
            var resolved = false;
            var assemblyName = targetSymbol.ContainingAssembly?.Name;
            foreach (var project in solution.Projects)
            {
                if (!string.Equals(project.AssemblyName, assemblyName, StringComparison.OrdinalIgnoreCase))
                    continue;

                var compilation = await project.GetCompilationAsync().ConfigureAwait(false);
                if (compilation is null)
                    continue;

                var picked = PickWorkspaceSymbol(
                    targetSymbol,
                    SymbolFinder.FindSimilarSymbols(targetSymbol, compilation),
                    out var fellBack);
                if (fellBack)
                    continue;

                workspaceSymbol = picked;
                resolved = true;
                break;
            }

            if (!resolved)
            {
                Console.Error.WriteLine(
                    $"[INFO] No workspace counterpart for {targetSymbol.Name}; resolving references with the detached symbol");
            }

            var refResults = await SymbolFinder
                .FindReferencesAsync(workspaceSymbol, solution)
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
            warnings.Add(
                $"FindReferencesAsync failed for {targetSymbol.Name}: " +
                $"{ex.GetType().Name}: {ex.Message}");
        }

        return (results, warnings);
    }
}
