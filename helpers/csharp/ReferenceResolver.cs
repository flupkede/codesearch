using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.FindSymbols;

namespace ScipCsharp;

/// <summary>
/// Resolves references for a single SCIP symbol on demand.
///
/// Used by the `find-refs` subcommand: the workspace is already loaded (same
/// startup cost as `index`), but instead of calling FindReferencesAsync for
/// every symbol in the solution, we call it once for the requested symbol only.
///
/// The Rust host caches the result in LMDB so subsequent find_impact calls for
/// the same symbol are instant (O(1) LMDB read).
/// </summary>
public sealed class ReferenceResolver
{
    /// <summary>
    /// Resolve all references for a single SCIP symbol key within the solution.
    /// </summary>
    /// <param name="solution">Fully loaded Roslyn solution.</param>
    /// <param name="scipKey">Canonical SCIP key, e.g. "csharp App . FieldDefinition#Validate()."</param>
    /// <param name="projectFilter">Optional project name / .csproj filename to limit compilation scope.</param>
    public async Task<FindRefsOutput> FindRefsAsync(
        Solution solution,
        string scipKey,
        string? projectFilter)
    {
        var output = new FindRefsOutput { Symbol = scipKey };

        // Build symbol map (fast — no FindReferencesAsync, just compilation + symbol walk).
        // We build from all projects so the SCIP key can always be resolved regardless of
        // which project owns the symbol. The projectFilter only limits the initial compilation
        // scope; FindReferencesAsync below still searches the full solution.
        var symbolMap = new Dictionary<ISymbol, string>(SymbolEqualityComparer.Default);

        Console.Error.WriteLine($"find-refs: building symbol map from solution...");
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

        // Find the ISymbol that matches the requested SCIP key.
        // SymbolIndexer.SymbolToScipName is the canonical key generator,
        // so an exact string comparison is sufficient.
        ISymbol? targetSymbol = null;
        foreach (var kv in symbolMap)
        {
            if (kv.Value == scipKey)
            {
                targetSymbol = kv.Key;
                break;
            }
        }

        if (targetSymbol is null)
        {
            Console.Error.WriteLine($"[WARN] find-refs: symbol not found in map: {scipKey}");
            return output;
        }

        // Find project root for relative path computation (mirrors SymbolIndexer.IndexAsync).
        var projectRoot = SymbolIndexer.FindCommonRoot(
            solution.Projects
                .Select(p => p.FilePath)
                .Where(p => p is not null)
                .Cast<string>());

        Console.Error.WriteLine($"find-refs: resolving references for {scipKey}...");

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
                output.References.Add(new FindRefsOccurrence
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
                $"[WARN] find-refs: FindReferencesAsync failed for {scipKey}: " +
                $"{ex.GetType().Name}: {ex.Message}");
        }

        Console.Error.WriteLine($"find-refs: found {output.References.Count} reference(s)");
        return output;
    }
}
