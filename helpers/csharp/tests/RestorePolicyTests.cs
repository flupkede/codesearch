using ScipCsharp;
using Xunit;

namespace ScipCsharp.Tests;

/// <summary>
/// Unit tests for the restore gating policy: a repo whose projects all carry
/// fresh obj/project.assets.json must skip restore entirely (the steady state
/// of an actively developed repo), while a missing or stale assets file
/// triggers it.
/// </summary>
public class RestorePolicyTests : IDisposable
{
    private readonly string _solutionDir;

    public RestorePolicyTests()
    {
        _solutionDir = Path.Combine(Path.GetTempPath(), "cs-restore-policy-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(_solutionDir);
    }

    public void Dispose()
    {
        Directory.Delete(_solutionDir, recursive: true);
    }

    private string AddProject(string relativePath, bool withAssets, bool staleAssets = false)
    {
        var projectPath = Path.Combine(_solutionDir, relativePath.Replace('\\', Path.DirectorySeparatorChar));
        Directory.CreateDirectory(Path.GetDirectoryName(projectPath)!);
        File.WriteAllText(projectPath, "<Project Sdk=\"Microsoft.NET.Sdk\" />");
        if (withAssets)
        {
            var objDir = Path.Combine(Path.GetDirectoryName(projectPath)!, "obj");
            Directory.CreateDirectory(objDir);
            var assetsPath = Path.Combine(objDir, "project.assets.json");
            File.WriteAllText(assetsPath, "{}");
            if (staleAssets)
            {
                // Make the assets look OLDER than the project file.
                File.SetLastWriteTimeUtc(assetsPath, File.GetLastWriteTimeUtc(projectPath).AddMinutes(-5));
            }
        }
        return projectPath;
    }

    [Fact]
    public void FreshAssetsOnEveryProjectSkipsRestore()
    {
        AddProject(@"src\Acme.Core\Acme.Core.csproj", withAssets: true);
        AddProject(@"src\Acme.Web\Acme.Web.csproj", withAssets: true);

        Assert.False(RestorePolicy.NeedsRestore(
            new[] { @"src\Acme.Core\Acme.Core.csproj", @"src\Acme.Web\Acme.Web.csproj" }, _solutionDir));
    }

    [Fact]
    public void MissingAssetsTriggersRestore()
    {
        AddProject(@"src\Acme.Core\Acme.Core.csproj", withAssets: true);
        AddProject(@"src\Acme.Web\Acme.Web.csproj", withAssets: false);

        Assert.True(RestorePolicy.NeedsRestore(
            new[] { @"src\Acme.Core\Acme.Core.csproj", @"src\Acme.Web\Acme.Web.csproj" }, _solutionDir));
    }

    [Fact]
    public void StaleAssetsOlderThanProjectTriggerRestore()
    {
        AddProject(@"src\Acme.Core\Acme.Core.csproj", withAssets: true, staleAssets: true);

        Assert.True(RestorePolicy.NeedsRestore(new[] { @"src\Acme.Core\Acme.Core.csproj" }, _solutionDir));
    }

    [Fact]
    public void EmptyProjectListSkipsRestore()
    {
        Assert.False(RestorePolicy.NeedsRestore(Array.Empty<string>(), _solutionDir));
    }

    [Fact]
    public void RestoreTimeoutDefaultsTo300AndParsesEnvOverride()
    {
        Assert.Equal(300, RestorePolicy.RestoreTimeoutSeconds());

        Environment.SetEnvironmentVariable(RestorePolicy.TimeoutEnvName, "600");
        try
        {
            Assert.Equal(600, RestorePolicy.RestoreTimeoutSeconds());
        }
        finally
        {
            Environment.SetEnvironmentVariable(RestorePolicy.TimeoutEnvName, null);
        }

        Environment.SetEnvironmentVariable(RestorePolicy.TimeoutEnvName, "not-a-number");
        try
        {
            Assert.Equal(300, RestorePolicy.RestoreTimeoutSeconds());
        }
        finally
        {
            Environment.SetEnvironmentVariable(RestorePolicy.TimeoutEnvName, null);
        }
    }

    /// <summary>
    /// Pins the timeout contract for the serve path: the wait expiring must
    /// announce a restore that is STILL RUNNING in the background, never a
    /// killed one — a kill would waste all of the restore's work — and name
    /// the gating that index/find-refs runs apply while it finishes.
    /// </summary>
    /// <summary>
    /// Pins the timeout contract for the serve path: the wait expiring must
    /// announce a restore that is STILL RUNNING in the background, never a
    /// killed one — a kill would waste all of the restore's work — and name
    /// the gating that index/find-refs runs apply while it finishes.
    /// Severity is [INFO]: restore state must not drive the warning icon.
    /// </summary>
    [Fact]
    public void TimeoutWarningAnnouncesBackgroundContinuationNeverAKill()
    {
        var warning = RestorePolicy.StillRunningWarning(300);

        Assert.StartsWith("[INFO]", warning);
        Assert.Contains("still running after 300s", warning);
        Assert.Contains("background", warning);
        Assert.Contains("gated", warning);
        Assert.Contains("fresh assets", warning);
        Assert.DoesNotContain("[WARN]", warning);
        Assert.DoesNotContain("killed", warning, StringComparison.OrdinalIgnoreCase);
        Assert.Contains(RestorePolicy.TimeoutEnvName, warning);
    }

    private string AddRestoreInput(string relativePath, bool touched) =>
        AddRestoreInputAt(Path.Combine(_solutionDir, relativePath.Replace('\\', Path.DirectorySeparatorChar)), touched);

    private string AddRestoreInputAt(string inputPath, bool touched)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(inputPath)!);
        File.WriteAllText(inputPath, "<Project />");
        File.SetLastWriteTimeUtc(
            inputPath,
            DateTime.UtcNow.AddMinutes(touched ? 5 : -5));
        return inputPath;
    }

    /// <summary>
    /// Central package management: bumping a version in
    /// Directory.Packages.props must mark the assets stale even though no
    /// .csproj changed — the CS0246 class of false-fresh assets.
    /// </summary>
    [Fact]
    public void TouchedPackagesPropsAtSolutionRootTriggersRestore()
    {
        AddProject(@"src\Acme.Core\Acme.Core.csproj", withAssets: true);
        AddRestoreInput(@"Directory.Packages.props", touched: true);

        Assert.True(RestorePolicy.NeedsRestore(
            new[] { @"src\Acme.Core\Acme.Core.csproj" }, _solutionDir));
    }

    /// <summary>
    /// A failed restore must carry its OWN reason (NU11xx feed/resolution
    /// errors) into the [WARN] line: without it the operator sees only the
    /// symptom — unresolved packages, CS0518/CS0246 cascades — and never
    /// learns the cause was e.g. an unreachable private feed.
    /// </summary>
    [Fact]
    public void RestoreFailureDetailExtractsNuErrorsCapsAndFiltersWarnings()
    {
        var output = string.Join('\n',
            "C:\\repo\\A.csproj : warning NU1900: Unable to load the service index for source https://feed.example",
            "C:\\repo\\B.csproj : error NU1101: Unable to find package Acme.Shared. No packages exist with this id in source(s): acme-feed.",
            "C:\\repo\\C.csproj : error NU1101: Unable to find package Other.",
            new string('x', 400) + " error NU1001 tail",
            "Done.");

        var detail = RestorePolicy.ExtractRestoreErrors(output);

        Assert.StartsWith(" Restore output: ", detail);
        Assert.Contains("NU1101: Unable to find package Acme.Shared", detail);
        Assert.Contains("Unable to find package Other", detail);
        Assert.DoesNotContain("NU1900", detail);
        Assert.DoesNotContain("Done.", detail);
        Assert.True(detail.Length < 800, "per-line caps keep the [WARN] single-line sized");
        Assert.Contains("...", detail);
    }

    [Fact]
    public void RestoreFailureDetailIsEmptyWithoutErrorLines()
    {
        Assert.Equal(string.Empty, RestorePolicy.ExtractRestoreErrors(""));
        Assert.Equal(string.Empty, RestorePolicy.ExtractRestoreErrors("   \n  "));
        Assert.Equal(string.Empty, RestorePolicy.ExtractRestoreErrors("only warnings\nand info"));
    }

    /// <summary>
    /// The skip-guard must hit exactly the dead netcoreapp line: a legacy
    /// 2.2 project design-time-loads into a CS0518 cascade on a modern SDK
    /// (tool-side gap, not code health), while netstandard libraries and the
    /// supported net line stay untouched.
    /// </summary>
    [Fact]
    public void EndOfLifeFrameworkClassificationTable()
    {
        Assert.True(SkippedFrameworks.IsEndOfLife("netcoreapp2.2"));
        Assert.True(SkippedFrameworks.IsEndOfLife("netcoreapp2.0"));
        Assert.True(SkippedFrameworks.IsEndOfLife("netcoreapp3.1"));
        Assert.True(SkippedFrameworks.IsEndOfLife(" NetCoreApp2.2 "));

        Assert.False(SkippedFrameworks.IsEndOfLife("net10.0"));
        Assert.False(SkippedFrameworks.IsEndOfLife("net48"));
        Assert.False(SkippedFrameworks.IsEndOfLife("netstandard2.0"));
        Assert.False(SkippedFrameworks.IsEndOfLife("netcoreapp"));
    }

    [Fact]
    public void TargetFrameworkExtractionFromProjectText()
    {
        Assert.Equal("netcoreapp2.2", SkippedFrameworks.ExtractFromProjectText(
            "<Project><PropertyGroup><TargetFramework>netcoreapp2.2</TargetFramework></PropertyGroup></Project>"));
        // Multi-target: the FIRST target decides (the one design-time uses).
        Assert.Equal("net10.0", SkippedFrameworks.ExtractFromProjectText(
            "<TargetFrameworks>net10.0;netstandard2.0</TargetFrameworks>"));
        Assert.Null(SkippedFrameworks.ExtractFromProjectText("<Project></Project>"));
    }

    [Fact]
    public void HasUsableAssetsFollowsTheAssetsFile()
    {
        AddProject(@"src\Acme.Core\Acme.Core.csproj", withAssets: true);
        Assert.True(RestorePolicy.HasUsableAssets(@"src\Acme.Core\Acme.Core.csproj", _solutionDir));

        AddProject(@"src\Acme.Bare\Acme.Bare.csproj", withAssets: false);
        Assert.False(RestorePolicy.HasUsableAssets(@"src\Acme.Bare\Acme.Bare.csproj", _solutionDir));
    }

    [Fact]
    public void UntouchedPackagesPropsKeepsRestoreSkipped()
    {
        AddProject(@"src\Acme.Core\Acme.Core.csproj", withAssets: true);
        AddRestoreInput(@"Directory.Packages.props", touched: false);

        Assert.False(RestorePolicy.NeedsRestore(
            new[] { @"src\Acme.Core\Acme.Core.csproj" }, _solutionDir));
    }

    /// <summary>
    /// The walk must extend ABOVE the solution dir: a Directory.Build.props at
    /// the repo root applies to a solution sitting deeper in the repo.
    /// </summary>
    [Fact]
    public void InputAboveSolutionDirAtRepoRootTriggersRestore()
    {
        var repoDir = Directory.CreateDirectory(Path.Combine(_solutionDir, "repo")).FullName;
        var solutionDir = Directory.CreateDirectory(Path.Combine(repoDir, "sln")).FullName;
        Directory.CreateDirectory(Path.Combine(repoDir, ".git"));

        var projectPath = Path.Combine(solutionDir, "src", "Acme.Core", "Acme.Core.csproj");
        Directory.CreateDirectory(Path.GetDirectoryName(projectPath)!);
        File.WriteAllText(projectPath, "<Project Sdk=\"Microsoft.NET.Sdk\" />");
        var objDir = Path.Combine(Path.GetDirectoryName(projectPath)!, "obj");
        Directory.CreateDirectory(objDir);
        File.WriteAllText(Path.Combine(objDir, "project.assets.json"), "{}");

        AddRestoreInputAt(Path.Combine(repoDir, "Directory.Build.props"), touched: true);

        Assert.True(RestorePolicy.NeedsRestore(
            new[] { @"src\Acme.Core\Acme.Core.csproj" }, solutionDir));
    }

    [Fact]
    public void TouchedNuGetConfigAndGlobalJsonTriggerRestore()
    {
        AddProject(@"src\Acme.Core\Acme.Core.csproj", withAssets: true);
        AddRestoreInput(@"NuGet.config", touched: true);
        AddRestoreInput(@"global.json", touched: false);

        Assert.True(RestorePolicy.NeedsRestore(
            new[] { @"src\Acme.Core\Acme.Core.csproj" }, _solutionDir));
    }

    /// <summary>
    /// Pins the case-insensitive input-name match: NuGet honours lowercase
    /// nuget.config on case-sensitive filesystems, where an exact-case probe
    /// would miss it.
    /// </summary>
    [Fact]
    public void CaseVariantInputNameStillTriggersRestore()
    {
        AddProject(@"src\Acme.Core\Acme.Core.csproj", withAssets: true);
        AddRestoreInput(@"nuget.config", touched: true);

        Assert.True(RestorePolicy.NeedsRestore(
            new[] { @"src\Acme.Core\Acme.Core.csproj" }, _solutionDir));
    }
}
