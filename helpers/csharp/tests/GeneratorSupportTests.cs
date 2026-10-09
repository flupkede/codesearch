using System.Diagnostics;
using Microsoft.Build.Locator;
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
    private static readonly object RegistrationLock = new();
    private static bool _msbuildRegistered;

    private static MSBuildWorkspace CreateWorkspace()
    {
        lock (RegistrationLock)
        {
            if (!_msbuildRegistered)
            {
                var instances = MSBuildLocator.QueryVisualStudioInstances().ToList();
                var sdk = instances.FirstOrDefault(i => i.DiscoveryType == DiscoveryType.DotNetSdk)
                          ?? instances.OrderByDescending(i => i.Version).First();
                MSBuildLocator.RegisterInstance(sdk);
                Environment.SetEnvironmentVariable("MSBUILD_EXE_PATH", sdk.MSBuildPath);
                _msbuildRegistered = true;
            }
        }

        return MSBuildWorkspace.Create(WorkspaceLoad.GlobalProperties());
    }

    private static async Task<(Project Project, MSBuildWorkspace Workspace)> OpenFixtureAsync()
    {
        var projectPath = Path.Combine(AppContext.BaseDirectory, "Fixtures", "BlazorLib", "BlazorLib.csproj");
        Assert.True(File.Exists(projectPath), $"fixture not copied to test output: {projectPath}");
        await RestoreFixtureIfNeededAsync(projectPath);

        var workspace = CreateWorkspace();
        // Design-time builds emit benign MSBuild warnings; surface, don't fail.
        workspace.WorkspaceFailed += (_, e) =>
            Console.Error.WriteLine($"[INFO] test workspace: {e.Diagnostic.Message}");
        var project = await workspace.OpenProjectAsync(projectPath);
        return (project, workspace);
    }

    private static async Task RestoreFixtureIfNeededAsync(string projectPath)
    {
        var assets = Path.Combine(Path.GetDirectoryName(projectPath)!, "obj", "project.assets.json");
        if (File.Exists(assets))
            return;

        var psi = new ProcessStartInfo("dotnet", $"restore \"{projectPath}\"")
        {
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            UseShellExecute = false,
            CreateNoWindow = true,
        };
        using var process = Process.Start(psi)!;
        var stderrTask = process.StandardError.ReadToEndAsync();
        var stdoutTask = process.StandardOutput.ReadToEndAsync();
        if (!process.WaitForExit(180_000))
        {
            process.Kill(entireProcessTree: true);
            throw new InvalidOperationException($"dotnet restore timed out for {projectPath}");
        }
        if (process.ExitCode != 0)
        {
            throw new InvalidOperationException(
                $"dotnet restore failed for {projectPath}:\n{await stderrTask}");
        }
        _ = await stdoutTask;
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
