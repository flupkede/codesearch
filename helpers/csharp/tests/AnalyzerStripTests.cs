using ScipCsharp;
using Xunit;

namespace ScipCsharp.Tests;

/// <summary>
/// Analyzer-strip unit tests: NU1101/NU1102 extraction (including the
/// verbatim incident wording), the analyzer-shape classifier (the card's
/// guard: only provably analyzer-only references are strippable), targets
/// content, and one real end-to-end restore over a local empty feed proving
/// an unresolvable analyzer package no longer fails the restore — with the
/// csproj byte-identical afterwards.
/// </summary>
public class AnalyzerStripTests : IDisposable
{
    private readonly string _root;

    public AnalyzerStripTests()
    {
        _root = Path.Combine(Path.GetTempPath(), "cs-analyzer-strip-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(_root);
    }

    public void Dispose()
    {
        try { Directory.Delete(_root, recursive: true); } catch { /* best effort */ }
    }

    // ── ExtractUnresolvedPackages ─────────────────────────────────────

    [Fact]
    public void Nu1101IncidentWordingExtractsId()
    {
        const string output = "src/Core/Core.csproj : error NU1101: Unable to find package " +
            "SonarAnalyzer.CSharp. No packages exist with this id in source(s): nuget.org";

        Assert.Equal(["SonarAnalyzer.CSharp"], AnalyzerStrip.ExtractUnresolvedPackages(output));
    }

    [Fact]
    public void QuotedAndN1102FormsExtractAndDeduplicate()
    {
        const string output = "error NU1101: Unable to find package 'Vendor.Analyzers' with " +
            "version (>= 1.0.0)\nerror NU1101: Unable to find package Vendor.Analyzers. No " +
            "packages exist with this id in source(s): feed\nerror NU1102: Unable to find " +
            "package Other.Analyzers with version (>= 2.0.0)";

        Assert.Equal(
            ["Vendor.Analyzers", "Other.Analyzers"],
            AnalyzerStrip.ExtractUnresolvedPackages(output));
    }

    [Theory]
    [InlineData("")]
    [InlineData("error NU1301: Unable to load the service index for source https://feed.")]
    [InlineData("error NETSDK1045: The current .NET SDK does not support targeting .NET 10.0.")]
    public void OtherFailureClassesYieldNothing(string output)
    {
        Assert.Empty(AnalyzerStrip.ExtractUnresolvedPackages(output));
    }

    // ── ElementLooksAnalyzerOnly ──────────────────────────────────────

    private const string SonarShapedElement = """
        <PackageReference Include="SonarAnalyzer.CSharp" Version="9.32.0.97167">
            <PrivateAssets>all</PrivateAssets>
            <IncludeAssets>runtime; build; native; contentfiles; analyzers; buildtransitive</IncludeAssets>
        </PackageReference>
        """;

    [Fact]
    public void MetadataBlockMentioningAnalyzersIsAnalyzerOnly() =>
        Assert.True(AnalyzerStrip.ElementLooksAnalyzerOnly(SonarShapedElement));

    [Fact]
    public void PrivateAssetsAllAttributeIsAnalyzerOnly() =>
        Assert.True(AnalyzerStrip.ElementLooksAnalyzerOnly(
            @"<PackageReference Include=""X"" Version=""1.0"" PrivateAssets=""all"" />"));

    [Theory]
    [InlineData(@"<PackageReference Include=""X"" Version=""1.0"" />")]
    [InlineData(@"<PackageReference Include=""Newtonsoft.Json"" Version=""13.0.3"" />")]
    public void BareOrRuntimeReferencesAreNotAnalyzerOnly(string element) =>
        Assert.False(AnalyzerStrip.ElementLooksAnalyzerOnly(element));

    // ── SelectStrippable ──────────────────────────────────────────────

    private void WriteProject(string name, string content)
    {
        Directory.CreateDirectory(Path.Combine(_root, name));
        File.WriteAllText(Path.Combine(_root, name, name + ".csproj"), content);
    }

    [Fact]
    public void SonarShapedReferenceIsStrippable()
    {
        WriteProject("Core", $@"<Project>{SonarShapedElement}</Project>");

        Assert.Equal(
            ["SonarAnalyzer.CSharp"],
            AnalyzerStrip.SelectStrippable(["SonarAnalyzer.CSharp"], _root));
    }

    [Fact]
    public void BareCpmReferenceIsNotStrippable()
    {
        WriteProject("Core",
            @"<Project><PackageReference Include=""Missing.Analyzers"" /></Project>");

        Assert.Empty(AnalyzerStrip.SelectStrippable(["Missing.Analyzers"], _root));
    }

    [Fact]
    public void PackageReferencedByNoProjectIsNotStrippable()
    {
        Assert.Empty(AnalyzerStrip.SelectStrippable(["Ghost.Analyzers"], _root));
    }

    [Fact]
    public void OneNonAnalyzerReferenceKeepsThePackage()
    {
        WriteProject("Core", $@"<Project>{SonarShapedElement}</Project>");
        WriteProject("Web",
            @"<Project><PackageReference Include=""SonarAnalyzer.CSharp"" Version=""9.32.0"" /></Project>");

        Assert.Empty(AnalyzerStrip.SelectStrippable(["SonarAnalyzer.CSharp"], _root));
    }

    [Fact]
    public void EmptyInputIsStrippableFree()
    {
        Assert.Empty(AnalyzerStrip.SelectStrippable([], _root));
    }

    // ── TargetsFileContent ────────────────────────────────────────────

    [Fact]
    public void ContentRemovesEachId()
    {
        var content = AnalyzerStrip.TargetsFileContent(["A.Analyzers", "B.Analyzers"]);

        Assert.Contains(@"<PackageReference Remove=""A.Analyzers"" />", content);
        Assert.Contains(@"<PackageReference Remove=""B.Analyzers"" />", content);
        Assert.Contains("<ItemGroup>", content);
    }

    [Fact]
    public void ContentEscapesXmlSpecials()
    {
        var content = AnalyzerStrip.TargetsFileContent(["Weird<&>.Pkg"]);

        Assert.Contains(@"Remove=""Weird&lt;&amp;&gt;.Pkg""", content);
    }

    // ── Stale sweep ───────────────────────────────────────────────────

    /// <summary>
    /// A crashed run's strip targets file (load-scope delete never ran) is
    /// swept by the same hygiene pass as filtered-solution leftovers.
    /// </summary>
    [Fact]
    public void StaleStripTargetsAreSwept()
    {
        var targets = Path.Combine(_root, "sln-strip-analyzers.targets");
        File.WriteAllText(targets, "<Project></Project>");

        Assert.Equal(1, WorkspaceLoad.CleanupStaleFilteredSolutions(_root, "sln"));
        Assert.False(File.Exists(targets));
    }

    // ── Real restore end-to-end ───────────────────────────────────────

    /// <summary>
    /// The card's acceptance shape: a project referencing an unresolvable
    /// analyzer-only package fails a plain restore, and the strip-targets
    /// retry succeeds with usable assets — csproj byte-identical throughout.
    /// A local empty feed (NuGet.config) keeps it offline-deterministic: the
    /// source resolves, the package genuinely does not exist, which is
    /// NU1101 even under --ignore-failed-sources.
    /// </summary>
    [Fact]
    public void StripTargetsMakeAnUnresolvableAnalyzerRestoreSucceed()
    {
        if (!DotnetCliAvailable())
        {
            return; // No CLI on PATH — the suite cannot exercise real restores.
        }
        var projDir = Path.Combine(_root, "proj");
        Directory.CreateDirectory(projDir);
        Directory.CreateDirectory(Path.Combine(projDir, "feed"));
        File.WriteAllText(Path.Combine(projDir, "NuGet.config"),
            """
            <?xml version="1.0" encoding="utf-8"?>
            <configuration>
              <packageSources>
                <clear />
                <add key="local-empty" value="feed" />
              </packageSources>
            </configuration>
            """);
        var csprojPath = Path.Combine(projDir, "proj.csproj");
        File.WriteAllText(csprojPath, $"""
            <Project Sdk="Microsoft.NET.Sdk">
              <PropertyGroup>
                <TargetFramework>net8.0</TargetFramework>
                <Nullable>disable</Nullable>
              </PropertyGroup>
              <ItemGroup>
                {SonarShapedElement.Replace("SonarAnalyzer.CSharp", "Missing.Analyzer.Package.X")}
              </ItemGroup>
            </Project>
            """);
        var csprojBefore = File.ReadAllBytes(csprojPath);

        // 1) Plain restore: NU1101 — today's degradation trigger.
        var plain = RunDotnet($"restore \"{csprojPath}\" --nologo -v q --ignore-failed-sources");
        Assert.NotEqual(0, plain.ExitCode);

        // 2) Strip-targets retry: succeeds, assets written.
        var targetsPath = Path.Combine(_root, "proj-strip-analyzers.targets");
        File.WriteAllText(targetsPath, AnalyzerStrip.TargetsFileContent(["Missing.Analyzer.Package.X"]));
        var stripped = RunDotnet(
            $"restore \"{csprojPath}\" --nologo -v q --ignore-failed-sources " +
            $"-p:CustomBeforeMicrosoftCommonTargets=\"{targetsPath}\"");
        Assert.Equal(0, stripped.ExitCode);
        Assert.True(File.Exists(Path.Combine(projDir, "obj", "project.assets.json")));

        // 3) The repo file was never touched.
        Assert.Equal(csprojBefore, File.ReadAllBytes(csprojPath));
    }

    private static (int ExitCode, string Output) RunDotnet(string arguments)
    {
        var psi = new System.Diagnostics.ProcessStartInfo
        {
            FileName = "dotnet",
            Arguments = arguments,
            RedirectStandardError = true,
            RedirectStandardOutput = true,
            UseShellExecute = false,
        };
        using var process = System.Diagnostics.Process.Start(psi)!;
        var output = process.StandardError.ReadToEnd() + process.StandardOutput.ReadToEnd();
        process.WaitForExit(180_000);
        return (process.ExitCode, output);
    }

    private static bool DotnetCliAvailable()
    {
        try
        {
            var (_, output) = RunDotnet("--version");
            return Version.TryParse(output.Trim().Split('.')[0], out _);
        }
        catch (Exception)
        {
            return false;
        }
    }
}
