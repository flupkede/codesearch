using ScipCsharp;
using Xunit;

namespace ScipCsharp.Tests;

/// <summary>
/// Unit tests for the VS-MSBuild restore fallback's pure logic: SDK-
/// resolution failure detection (the feature's trigger — pinned against
/// the exact host wordings seen live on 2026-10-15, incl. NETSDK1045
/// printing to stdout), vswhere -find output parsing, the capability-aware
/// MSBuild instance choice, and the repo TFM floor scan.
/// </summary>
public class MsbuildRestoreTests : IDisposable
{
    private readonly string _root;

    public MsbuildRestoreTests()
    {
        _root = Path.Combine(Path.GetTempPath(), "cs-msbuild-restore-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(_root);
    }

    public void Dispose()
    {
        Directory.Delete(_root, recursive: true);
    }

    // ── IsSdkResolutionFailure ────────────────────────────────────────

    [Fact]
    public void Netsdk1045StdoutWordingTriggers()
    {
        // Verbatim shape of the live net99.0 probe output (stdout), the
        // 2026-10-15 incident's failure mode.
        var stdout =
            "C:\\Program Files\\dotnet\\sdk\\8.0.425\\Sdks\\Microsoft.NET.Sdk\\targets\\" +
            "Microsoft.NET.TargetFrameworkInference.targets(166,5): error NETSDK1045: " +
            "The current .NET SDK does not support targeting .NET 10.0.  Either target " +
            ".NET 8.0 or lower, or use a version of the .NET SDK that supports .NET 10.0.";

        Assert.True(MsbuildRestore.IsSdkResolutionFailure(stdout));
    }

    [Theory]
    [InlineData("A compatible .NET SDK was not found. Install or update the .NET SDK.")] // current host
    [InlineData("A compatible installed .NET SDK for global.json at [C:\\repo\\global.json] was not found.")] // legacy host
    public void GlobalJsonUnsatisfiableWordingsTriggerBothHostGenerations(string line)
    {
        Assert.True(MsbuildRestore.IsSdkResolutionFailure(line));
    }

    [Theory]
    [InlineData("error NU1101: Unable to find package Vendor.Internal.Base. No packages exist with this id in source(s): private-feed.")]
    [InlineData("App.Core.csproj: error NU1301: Unable to load the service index for source https://pkgs.dev.azure.com/org/_packaging/private-feed/nuget/v3/index.json.")]
    [InlineData("")]
    public void FeedAndAuthFailuresDoNotTrigger(string line)
    {
        // VS restores through the same NuGet stack — retrying those buys a
        // second slow failure, so they must fall through to per-project.
        Assert.False(MsbuildRestore.IsSdkResolutionFailure(line));
    }

    // ── ParseFirstVswherePath ─────────────────────────────────────────

    [Fact]
    public void VswhereParsingSkipsBlankLines()
    {
        var output = "\r\nC:\\Program Files\\Microsoft Visual Studio\\18\\Professional\\MSBuild\\Current\\Bin\\MSBuild.exe\r\n\r\n";

        Assert.Equal(
            "C:\\Program Files\\Microsoft Visual Studio\\18\\Professional\\MSBuild\\Current\\Bin\\MSBuild.exe",
            MsbuildRestore.ParseFirstVswherePath(output));
    }

    [Fact]
    public void VswhereParsingEmptyOutputYieldsNull()
    {
        Assert.Null(MsbuildRestore.ParseFirstVswherePath(""));
    }

    // ── ChooseMsBuildInstance ─────────────────────────────────────────

    private static MsbuildCandidate Sdk(int major) => new(new Version(major, 0), IsDotNetSdk: true);

    private static MsbuildCandidate Vs(int major) => new(new Version(major, 0), IsDotNetSdk: false);

    [Fact]
    public void NoFloorKeepsHighestSdk()
    {
        var candidates = new[] { Sdk(8), Sdk(10), Vs(18) };

        var (index, isSdk) = MsbuildRestore.ChooseMsBuildInstance(candidates, requiredMajor: null);

        Assert.Equal((1, true), (index, isSdk));
    }

    [Fact]
    public void QualifyingSdkBeatsNewerVs()
    {
        var candidates = new[] { Sdk(10), Vs(18) };

        var (index, isSdk) = MsbuildRestore.ChooseMsBuildInstance(candidates, requiredMajor: 10);

        Assert.Equal((0, true), (index, isSdk));
    }

    [Fact]
    public void SdkOlderThanFloorFallsToVs()
    {
        // The incident layout: CLI SDK 8 only, repo targets net10, VS 18 present.
        var candidates = new[] { Sdk(8), Vs(18) };

        var (index, isSdk) = MsbuildRestore.ChooseMsBuildInstance(candidates, requiredMajor: 10);

        Assert.Equal((1, false), (index, isSdk));
    }

    [Fact]
    public void SdkOlderThanFloorWithoutVsKeepsSdkAsLastResort()
    {
        var candidates = new[] { Sdk(8) };

        var (index, isSdk) = MsbuildRestore.ChooseMsBuildInstance(candidates, requiredMajor: 10);

        Assert.Equal((0, true), (index, isSdk));
    }

    [Fact]
    public void HighestQualifyingSdkWinsAmongSdkSiblings()
    {
        var candidates = new[] { Sdk(8), Sdk(10), Sdk(11) };

        var (index, isSdk) = MsbuildRestore.ChooseMsBuildInstance(candidates, requiredMajor: 10);

        Assert.Equal((2, true), (index, isSdk));
    }

    // ── RequiredSdkMajorFloor ─────────────────────────────────────────

    private string WriteSolution(params (string RelativePath, string TargetFrameworks)[] projects)
    {
        var solutionLines = new List<string> { "Microsoft Visual Studio Solution File, Format Version 12.00", "Global" };
        foreach (var (relative, tfms) in projects)
        {
            var projectPath = Path.Combine(_root, relative.Replace('\\', Path.DirectorySeparatorChar));
            Directory.CreateDirectory(Path.GetDirectoryName(projectPath)!);
            File.WriteAllText(
                projectPath,
                $"<Project Sdk=\"Microsoft.NET.Sdk\">\n  <PropertyGroup>\n    <TargetFrameworks>{tfms}</TargetFrameworks>\n  </PropertyGroup>\n</Project>\n");
            solutionLines.Insert(
                1,
                $"Project(\"{{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}}\") = \"P\", \"{relative}\", \"{{GUID}}\"");
        }
        var solutionPath = Path.Combine(_root, "Repo.sln");
        File.WriteAllLines(solutionPath, solutionLines);
        return solutionPath;
    }

    [Fact]
    public void MultiTargetListFloorsAtHighestTfm()
    {
        // net8.0;net10.0 must floor at 10 — first-TFM (the pre-fix stance)
        // would floor at 8 and register an SDK 8 instance that cannot
        // evaluate the net10 half (the exact incident class). The pinned
        // 8.x SDK below must NOT lower it.
        var solutionPath = WriteSolution(("src\\A\\A.csproj", "net8.0;net10.0"));
        WriteGlobalJson(Path.Combine(_root, "global.json"), "8.0.100");

        Assert.Equal(10, MsbuildRestore.RequiredSdkMajorFloor(solutionPath));
    }

    [Fact]
    public void FloorTakesMaxAcrossProjectsAndGlobalJson()
    {
        var solutionPath = WriteSolution(
            ("src\\A\\A.csproj", "net8.0"),
            ("src\\B\\B.csproj", "net10.0"));
        WriteGlobalJson(Path.Combine(_root, "global.json"), "10.0.303");

        Assert.Equal(10, MsbuildRestore.RequiredSdkMajorFloor(solutionPath));
    }

    [Fact]
    public void GlobalJsonAboveSolutionDirRaisesTheFloor()
    {
        // NuGet's own lookup order: the sln and its projects sit in a
        // subdir, global.json at the parent — the walk-up must find the
        // pinned 11 over the projects' net8.
        var solutionPath = WriteSolution(("src\\A\\A.csproj", "net8.0"));
        var repoDir = Path.Combine(_root, "repo");
        Directory.CreateDirectory(repoDir);
        Directory.Move(Path.Combine(_root, "src"), Path.Combine(repoDir, "src"));
        File.Move(solutionPath, Path.Combine(repoDir, "Repo.sln"));
        WriteGlobalJson(Path.Combine(_root, "global.json"), "11.0.100");

        Assert.Equal(11, MsbuildRestore.RequiredSdkMajorFloor(Path.Combine(repoDir, "Repo.sln")));
    }

    [Fact]
    public void NonNetTargetsAndMissingSolutionYieldNoFloor()
    {
        // The local global.json pins nothing parseable, so no ancestor
        // stray file can perturb the null expectation.
        var solutionPath = WriteSolution(("src\\A\\A.csproj", "netstandard2.0"));
        WriteGlobalJson(Path.Combine(_root, "global.json"), "{}");

        Assert.Null(MsbuildRestore.RequiredSdkMajorFloor(solutionPath));
        Assert.Null(MsbuildRestore.RequiredSdkMajorFloor(null));
        Assert.Null(MsbuildRestore.RequiredSdkMajorFloor(Path.Combine(_root, "absent.sln")));
    }

    [Fact]
    public void NetFrameworkSpellingsDoNotRaiseTheFloor()
    {
        // net48/net472 (extension-less) must not become a "48" floor: an
        // SDK loads them from reference assemblies, and a 48 floor would
        // flip healthy SDK+VS machines to the VS instance for no reason.
        // The pre-fix regex (^net(\d+)(?:\.|$)) matched net48 as major 48.
        var net48Only = WriteSolution(("src\\Legacy\\Legacy.csproj", "net48"));

        Assert.Null(MsbuildRestore.RequiredSdkMajorFloor(net48Only));

        var mixed = WriteSolution(("src\\Mixed\\Mixed.csproj", "net48;net10.0"));

        Assert.Equal(10, MsbuildRestore.RequiredSdkMajorFloor(mixed));
    }

    private static void WriteGlobalJson(string path, string version)
    {
        var content = version.StartsWith('{')
            ? version
            : $"{{ \"sdk\": {{ \"version\": \"{version}\" }} }}";
        File.WriteAllText(path, content);
    }
}
