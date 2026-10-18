using Xunit;
using Xunit.Abstractions;
using ScipCsharp;

namespace ScipCsharp.Tests;

/// <summary>
/// The gate for the single-pass references emitted by `index`: for every
/// definition key they must equal, as a multiset of (file, start_line,
/// end_line), what the lazy `batch-find-refs` path (one FindReferencesAsync
/// per key) answers on the same solution.
/// </summary>
[Collection("msbuild-workspace")]
public class ReferencePassParityTests(ITestOutputHelper output)
{
    private sealed record KnownGap(string Missing, string Extra, string Reason);

    /// <summary>
    /// Accepted divergences, pinned to their exact sites so any further
    /// divergence on the same key still fails.
    /// </summary>
    private static readonly IReadOnlyDictionary<string, KnownGap> KnownGaps = new Dictionary<string, KnownGap>
    {
        ["csharp Parity.Core . Shape#"] = new("Core/Shapes.cs:39-39", "-",
            "FindReferencesAsync reports the primary-constructor base `: Shape(...)` token in both the type and the constructor group at the same span; exact duplicates are deduped"),
        ["csharp Parity.Core . Circle#"] = new("Consumer/Extras.cs:11-11", "-",
            "FindReferencesAsync reports a type reference written through a using alias twice at the same span; exact duplicates are deduped"),
        ["csharp Parity.Core . Session#Dispose()."] = new("Consumer/Extras.cs:27-27", "-",
            "FindReferencesAsync reports every initialized local declaration of a disposable type as an implicit Dispose call; only a `using` disposes"),
    };

    private static readonly string[] ProjectNames = ["Core", "Services", "Consumer"];

    internal static async Task<Microsoft.CodeAnalysis.MSBuild.MSBuildWorkspace> OpenParitySolutionAsync()
    {
        var solutionPath = MsbuildTestWorkspace.FixturePath("ParitySolution", "ParitySolution.sln");
        Assert.True(File.Exists(solutionPath), $"fixture not copied to test output: {solutionPath}");
        await MsbuildTestWorkspace.RestoreIfNeededAsync(
            solutionPath,
            ProjectNames.Select(p => MsbuildTestWorkspace.FixturePath("ParitySolution", p)).ToArray());

        var workspace = MsbuildTestWorkspace.Create();
        await workspace.OpenSolutionAsync(solutionPath);
        Assert.Equal(
            ProjectNames.OrderBy(n => n),
            workspace.CurrentSolution.Projects.Select(p => p.Name).OrderBy(n => n));
        return workspace;
    }

    [Fact]
    public async Task SinglePass_MatchesFindReferencesForEveryDefinitionKey()
    {
        using var workspace = await OpenParitySolutionAsync();

        var index = await new SymbolIndexer().IndexAsync(workspace, projectFilter: null);
        Assert.NotNull(index.References);
        var keys = index.ExternalSymbols.Select(s => s.Symbol).Distinct().ToList();
        var lazy = await new ReferenceResolver().BatchFindRefsAsync(workspace.CurrentSolution, keys);

        var single = index.References!
            .GroupBy(r => r.Symbol)
            .ToDictionary(g => g.Key, g => g.Select(r => (r.File, r.StartLine, r.EndLine)).ToList());
        Assert.Empty(single.Keys.Except(keys));

        var gaps = new List<string>();
        var allowed = new List<string>();
        foreach (var result in lazy.Results)
        {
            Assert.True(result.Warnings.Count == 0,
                $"lazy answer for {result.Symbol} is partial: {string.Join(" | ", result.Warnings)}");
            var expected = result.References.Select(r => (r.File, r.StartLine, r.EndLine)).ToList();
            var actual = single.GetValueOrDefault(result.Symbol) ?? [];
            output.WriteLine($"{expected.Count,3} lazy / {actual.Count,3} single  {result.Symbol}");

            var missing = MultisetMinus(expected, actual);
            var extra = MultisetMinus(actual, expected);
            if (missing.Count == 0 && extra.Count == 0)
                continue;
            var line = $"{result.Symbol}\n    missing-in-single-pass: {Format(missing)}\n    extra-in-single-pass:   {Format(extra)}";
            if (KnownGaps.TryGetValue(result.Symbol, out var gap) && gap.Missing == Format(missing) && gap.Extra == Format(extra))
                allowed.Add(result.Symbol);
            else
                gaps.Add(line);
        }

        foreach (var key in allowed)
            output.WriteLine($"ALLOWED GAP {key}: {KnownGaps[key].Reason}");
        Assert.True(gaps.Count == 0, $"{gaps.Count} key(s) diverge from FindReferencesAsync:\n" + string.Join("\n", gaps));
        var staleAllowances = KnownGaps.Keys.Except(allowed).ToList();
        Assert.True(staleAllowances.Count == 0, "allow-list entries that no longer diverge: " + string.Join(", ", staleAllowances));
    }

    private static List<(string File, int StartLine, int EndLine)> MultisetMinus(
        List<(string File, int StartLine, int EndLine)> left,
        List<(string File, int StartLine, int EndLine)> right)
    {
        var remaining = right.GroupBy(r => r).ToDictionary(g => g.Key, g => g.Count());
        var result = new List<(string, int, int)>();
        foreach (var item in left)
        {
            if (remaining.TryGetValue(item, out var n) && n > 0)
                remaining[item] = n - 1;
            else
                result.Add(item);
        }
        return result;
    }

    private static string Format(List<(string File, int StartLine, int EndLine)> sites) =>
        sites.Count == 0
            ? "-"
            : string.Join(", ", sites.OrderBy(s => s.File, StringComparer.Ordinal).ThenBy(s => s.StartLine)
                .Select(s => $"{s.File}:{s.StartLine}-{s.EndLine}"));
}
