using System.Security.Cryptography;
using System.Text.Json;
using Microsoft.CodeAnalysis.MSBuild;
using Xunit;
using ScipCsharp;

namespace ScipCsharp.Tests;

/// <summary>
/// One restored + loaded copy of the SmallSolution fixture, shared by every
/// test of <see cref="ServeUpdateTests"/>. The copy lives under the test
/// output directory (so the repo global.json still pins the SDK) and is
/// separate from the shipped fixture, which tests must never mutate.
/// </summary>
public sealed class ServeUpdateFixture : IAsyncLifetime
{
    public string Root { get; } = Path.Combine(AppContext.BaseDirectory, "serve-update-" + Guid.NewGuid().ToString("N"));
    public MSBuildWorkspace Workspace { get; private set; } = null!;

    public string MainCs => Path.Combine(Root, "App", "Main.cs");
    public string CalculatorCs => Path.Combine(Root, "Library", "Calculator.cs");
    public string AppCsproj => Path.Combine(Root, "App", "App.csproj");

    public async Task InitializeAsync()
    {
        CopyWithoutBuildOutput(MsbuildTestWorkspace.FixturePath("SmallSolution"), Root);
        var solutionPath = Path.Combine(Root, "SmallSolution.sln");
        await MsbuildTestWorkspace.RestoreIfNeededAsync(
            solutionPath, Path.Combine(Root, "App"), Path.Combine(Root, "Library"));
        Workspace = MsbuildTestWorkspace.Create();
        await Workspace.OpenSolutionAsync(solutionPath);
        Assert.Equal(2, Workspace.CurrentSolution.Projects.Count());
    }

    public Task DisposeAsync()
    {
        Workspace.Dispose();
        Directory.Delete(Root, recursive: true);
        return Task.CompletedTask;
    }

    private static void CopyWithoutBuildOutput(string source, string target)
    {
        Directory.CreateDirectory(target);
        foreach (var file in Directory.EnumerateFiles(source))
            File.Copy(file, Path.Combine(target, Path.GetFileName(file)));
        foreach (var directory in Directory.EnumerateDirectories(source))
        {
            var name = Path.GetFileName(directory);
            if (name is "bin" or "obj")
                continue;
            CopyWithoutBuildOutput(directory, Path.Combine(target, name));
        }
    }
}

/// <summary>
/// The resident `update` op: file changes land in the held snapshot in place
/// (no reload), failures leave the snapshot untouched, and nothing is ever
/// written to disk. Requests go through the same JSON lines the Rust host
/// sends, answers are compared in their wire form.
/// </summary>
[Collection("msbuild-workspace")]
public sealed class ServeUpdateTests : IClassFixture<ServeUpdateFixture>, IDisposable
{
    private const string SubtractKey = "csharp SmallSolution.Library . Calculator#Subtract(int, int).";
    private const string CallSite = "int diff = calc.Subtract(10, 3);";

    private readonly ServeUpdateFixture _fixture;
    private readonly ServeHost _host;
    private readonly Dictionary<string, string> _sourcesAtStart;

    public ServeUpdateTests(ServeUpdateFixture fixture)
    {
        _fixture = fixture;
        _host = new ServeHost(
            fixture.Workspace,
            _ => throw new InvalidOperationException("update must never trigger a reload"),
            defaultSolution: null);
        _sourcesAtStart = SourceFiles().ToDictionary(p => p, File.ReadAllText);
    }

    public void Dispose()
    {
        foreach (var path in SourceFiles().Except(_sourcesAtStart.Keys))
            File.Delete(path);
        foreach (var (path, text) in _sourcesAtStart)
            File.WriteAllText(path, text);
    }

    private IEnumerable<string> SourceFiles() =>
        Directory.EnumerateFiles(_fixture.Root, "*.cs", SearchOption.AllDirectories)
            .Where(p => !Path.GetRelativePath(_fixture.Root, p)
                .Split(Path.DirectorySeparatorChar)
                .Any(segment => segment is "bin" or "obj"));

    private async Task<string> UpdateAsync(string[] changed, string[] deleted)
    {
        var line = JsonSerializer.Serialize(new { op = "update", changed, deleted });
        return ServeHost.ToJson((await _host.HandleLineAsync(line))!);
    }

    private async Task<List<(string File, int Line)>> RefsAsync()
    {
        var line = JsonSerializer.Serialize(new { op = "find-refs", symbol = SubtractKey });
        var response = (await _host.HandleLineAsync(line))!;
        Assert.True(response.Ok, response.Error);
        Assert.Empty(response.Result!.Warnings);
        return response.Result.References
            .Select(r => (Path.GetFileName(r.File), r.StartLine))
            .OrderBy(r => r.Item1).ThenBy(r => r.StartLine)
            .ToList();
    }

    private static void ReplaceInFile(string path, string oldText, string newText)
    {
        var text = File.ReadAllText(path);
        Assert.Contains(oldText, text);
        File.WriteAllText(path, text.Replace(oldText, newText));
    }

    private string WriteExtraSource()
    {
        var path = Path.Combine(_fixture.Root, "App", "Extra.cs");
        File.WriteAllText(path,
            "using SmallSolution.Library;\nnamespace SmallSolution.App;\n" +
            "public static class Extra { public static int Run() => new Calculator().Subtract(2, 1); }\n");
        return path;
    }

    private static string Wire(object response) => JsonSerializer.Serialize(response);

    private Dictionary<string, string> HashTree() =>
        Directory.EnumerateFiles(_fixture.Root, "*", SearchOption.AllDirectories)
            .ToDictionary(
                p => Path.GetRelativePath(_fixture.Root, p),
                p => Convert.ToHexString(SHA256.HashData(File.ReadAllBytes(p))));

    [Fact]
    public async Task Baseline_FindsTheCallSitesInBothProjects()
    {
        Assert.Equal([("Calculator.cs", 54), ("Main.cs", 11)], await RefsAsync());
    }

    [Theory]
    [InlineData(CallSite + "\n        calc.Subtract(1, 1);", 2)]
    [InlineData("int diff = 10 - 3;", 0)]
    public async Task Update_ChangedCallSite_IsVisibleToFindRefsWithoutReload(string replacement, int mainCsRefsAfter)
    {
        ReplaceInFile(_fixture.MainCs, CallSite, replacement);

        var answer = await UpdateAsync([_fixture.MainCs], []);

        Assert.Equal(Wire(new { ok = true, applied = 1 }), answer);
        var after = await RefsAsync();
        Assert.Equal(mainCsRefsAfter, after.Count(r => r.File == "Main.cs"));
        Assert.Single(after, r => r.File == "Calculator.cs");
    }

    [Fact]
    public async Task Update_NewSourceFileInProjectDirectory_IsFound()
    {
        var added = WriteExtraSource();

        var answer = await UpdateAsync([added], []);

        Assert.Equal(Wire(new { ok = true, applied = 1 }), answer);
        Assert.Contains(("Extra.cs", 3), await RefsAsync());
    }

    [Fact]
    public async Task Update_DeletedFile_DropsItsReferences()
    {
        File.Delete(_fixture.MainCs);

        var answer = await UpdateAsync([], [_fixture.MainCs]);

        Assert.Equal(Wire(new { ok = true, applied = 1 }), answer);
        Assert.Equal([("Calculator.cs", 54)], await RefsAsync());
    }

    [Theory]
    [InlineData("App/App.csproj")]
    [InlineData("SmallSolution.sln")]
    [InlineData("SmallSolution.slnx")]
    [InlineData("Directory.Build.props")]
    [InlineData("Directory.Build.targets")]
    [InlineData("Directory.Packages.props")]
    [InlineData("packages.lock.json")]
    [InlineData("global.json")]
    public async Task Update_StructuralPath_NeedsReloadAndKeepsTheSnapshot(string relativePath)
    {
        var before = await RefsAsync();
        ReplaceInFile(_fixture.MainCs, CallSite, CallSite + "\n        calc.Subtract(1, 1);");
        var structural = Path.GetFullPath(Path.Combine(_fixture.Root, relativePath));

        var answer = await UpdateAsync([_fixture.MainCs, structural], []);

        Assert.Equal(Wire(new { ok = false, error = "needs-reload: " + structural }), answer);
        Assert.Equal(before, await RefsAsync());
    }

    [Fact]
    public async Task Update_FailureMidBatch_KeepsTheSnapshot()
    {
        var before = await RefsAsync();
        ReplaceInFile(_fixture.MainCs, CallSite, CallSite + "\n        calc.Subtract(1, 1);");
        var missing = Path.Combine(_fixture.Root, "App", "Missing.cs");

        var answer = await UpdateAsync([_fixture.MainCs, missing], []);

        Assert.Equal(Wire(new { ok = false, error = "needs-reload: " + missing }), answer);
        Assert.Equal(before, await RefsAsync());
    }

    [Fact]
    public async Task Update_UnknownNonSourcePath_IsIgnoredAndNotCounted()
    {
        var answer = await UpdateAsync([Path.Combine(_fixture.Root, "NOTES.md")], []);

        Assert.Equal(Wire(new { ok = true, applied = 0 }), answer);
    }

    [Fact]
    public async Task Update_CaseVariantOfADocumentPath_UpdatesItInsteadOfAddingADuplicate()
    {
        if (!OperatingSystem.IsWindows())
            return;
        ReplaceInFile(_fixture.MainCs, CallSite, CallSite + "\n        calc.Subtract(1, 1);");

        await UpdateAsync([_fixture.MainCs.ToUpperInvariant()], []);

        Assert.Equal(2, (await RefsAsync()).Count(r => r.File == "Main.cs"));
    }

    [Fact]
    public async Task Update_NeverWritesToDisk()
    {
        var added = WriteExtraSource();
        ReplaceInFile(_fixture.MainCs, CallSite, CallSite + "\n        calc.Subtract(1, 1);");
        var before = HashTree();
        Assert.Contains(Path.Combine("App", "App.csproj"), before.Keys);

        await UpdateAsync([_fixture.MainCs, added], []);
        await UpdateAsync([], [added]);
        await RefsAsync();

        Assert.Equal(before, HashTree());
    }
}
