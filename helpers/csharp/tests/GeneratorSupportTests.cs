using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.MSBuild;
using Xunit;
using ScipCsharp;

namespace ScipCsharp.Tests;

/// <summary>
/// Blazor end-to-end over a real MSBuild workspace: the Razor source generator
/// must run so the code-behind partial sees the @code members. Pins the false
/// "Compilation errors" CS0103 cascade that used to poison the index warnings
/// on every Razor project although `dotnet build` is clean.
/// </summary>
[Collection("msbuild-workspace")]
public class GeneratorSupportTests
{
    private static async Task<(Project Project, MSBuildWorkspace Workspace)> OpenFixtureAsync()
    {
        var projectPath = MsbuildTestWorkspace.FixturePath("BlazorLib", "BlazorLib.csproj");
        Assert.True(File.Exists(projectPath), $"fixture not copied to test output: {projectPath}");
        await MsbuildTestWorkspace.RestoreIfNeededAsync(projectPath);

        var workspace = MsbuildTestWorkspace.Create();
        var project = await workspace.OpenProjectAsync(projectPath);
        return (project, workspace);
    }

    [Fact]
    public async Task BlazorFixture_GeneratorRunResolvesCodeBehindMembers()
    {
        var (project, workspace) = await OpenFixtureAsync();
        using var _ = workspace;

        // Ground truth first: on the plain compilation the code-behind cannot
        // see the @code members — the exact defect this pins. If this assert
        // ever fails, the fixture no longer reproduces the problem.
        var plain = await project.GetCompilationAsync();
        Assert.NotNull(plain);
        Assert.Contains(plain!.GetDiagnostics(),
            d => d.Severity == DiagnosticSeverity.Error && d.Id == "CS0103");

        var (compilation, outcome) = await GeneratorSupport.GetCompilationWithGeneratorsAsync(project);
        Assert.Equal(GeneratorRunOutcome.Ran, outcome);
        Assert.NotNull(compilation);

        var errors = compilation!.GetDiagnostics()
            .Where(d => d.Severity == DiagnosticSeverity.Error)
            .ToList();
        Assert.True(errors.Count == 0,
            "expected zero compilation errors after the generator run, got: " +
            string.Join(" | ", errors.Take(5).Select(e => e.ToString())));

        // And the generated component type itself must be reachable through
        // the same symbol walk the indexer uses (it used to be missing
        // entirely, hiding every Razor component from the index).
        var symbolMap = new Dictionary<ISymbol, string>(SymbolEqualityComparer.Default);
        SymbolIndexer.CollectSymbols(compilation.GlobalNamespace, symbolMap);
        Assert.Contains(symbolMap.Keys.OfType<INamedTypeSymbol>(), t => t.Name == "Counter");
    }
}
