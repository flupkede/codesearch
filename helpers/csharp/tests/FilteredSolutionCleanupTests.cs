using ScipCsharp;
using Xunit;

namespace ScipCsharp.Tests;

/// <summary>
/// The filtered-.sln self-cleanup: leftovers from a crashed helper run (the
/// best-effort finally-delete never reached) are removed by name pattern at
/// the next solution load, case-insensitively, without ever throwing.
/// </summary>
public class FilteredSolutionCleanupTests : IDisposable
{
    private readonly string _root;

    public FilteredSolutionCleanupTests()
    {
        _root = Path.Combine(Path.GetTempPath(), "cs-filtered-cleanup-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(_root);
    }

    public void Dispose()
    {
        Directory.Delete(_root, recursive: true);
    }

    private void Write(string name)
    {
        File.WriteAllText(Path.Combine(_root, name), "solution content");
    }

    [Fact]
    public void DeletesStaleFilteredSolutionsAndCounts()
    {
        var staleUpperPrefix = Path.Combine(_root, "ACME-filtered-aaaa.sln"); // case-insensitive prefix
        var staleLower = Path.Combine(_root, "acme-filtered-bbbb.sln");
        var realSolution = Path.Combine(_root, "Acme.sln");
        Write("ACME-filtered-aaaa.sln");
        Write("acme-filtered-bbbb.sln");
        Write("Acme.sln");

        var deleted = WorkspaceLoad.CleanupStaleFilteredSolutions(_root, "Acme");

        Assert.Equal(2, deleted);
        Assert.False(File.Exists(staleUpperPrefix));
        Assert.False(File.Exists(staleLower));
        Assert.True(File.Exists(realSolution), "the real solution must never be deleted");
    }

    [Fact]
    public void PrefixMustMatchFullBaseName()
    {
        Write("Fake-filtered-cccc.sln"); // different base name
        Write("Acme-filtered-old.sln.bak"); // not a .sln

        var deleted = WorkspaceLoad.CleanupStaleFilteredSolutions(_root, "Acme");

        Assert.Equal(0, deleted);
    }

    [Fact]
    public void NoLeftoversDeletesNothing()
    {
        Write("Acme.sln");

        Assert.Equal(0, WorkspaceLoad.CleanupStaleFilteredSolutions(_root, "Acme"));
    }

    [Fact]
    public void MissingDirectoryNeverThrows()
    {
        var missing = Path.Combine(_root, "nope");

        Assert.Equal(0, WorkspaceLoad.CleanupStaleFilteredSolutions(missing, "Acme"));
        // A cleanup scan must not materialize the database dir under a
        // nonexistent solution dir — creation is the writer's job.
        Assert.False(Directory.Exists(Path.Combine(missing, ".codesearch.db")));
    }

    [Fact]
    public void StagingDirIsTheHiddenDatabaseTmp()
    {
        var dir = WorkspaceLoad.FilteredSolutionStagingDir(_root);

        Assert.Equal(Path.Combine(_root, ".codesearch.db", "tmp"), dir);
        Assert.True(Directory.Exists(dir));
        // The staging file lives inside the database dir the Rust side
        // hard-excludes (ALWAYS_EXCLUDED), so the watcher never sees it.
        Assert.StartsWith(
            Path.Combine(_root, ".codesearch.db"), dir, StringComparison.OrdinalIgnoreCase);
    }

    [Fact]
    public void DeletesStaleLeftoversInStagingDirToo()
    {
        var staging = WorkspaceLoad.FilteredSolutionStagingDir(_root);
        var staleInStaging = Path.Combine(staging, "Acme-filtered-dddd.sln");
        File.WriteAllText(staleInStaging, "solution content");
        Write("Acme.sln");

        var deleted = WorkspaceLoad.CleanupStaleFilteredSolutions(_root, "Acme");

        Assert.Equal(1, deleted);
        Assert.False(File.Exists(staleInStaging));
        Assert.True(File.Exists(Path.Combine(_root, "Acme.sln")));
    }

    [Fact]
    public void AdaptedStagedSolutionCarriesAbsolutePathsAndNoSolutionItems()
    {
        var sln = string.Join("\r\n",
            "Microsoft Visual Studio Solution File, Format Version 12.00",
            "Project(\"{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}\") = \"Core\", \"src\\Core\\Core.csproj\", \"{11111111-1111-1111-1111-111111111111}\"",
            "EndProject",
            "Project(\"{2150E333-8FDC-42A3-9474-1A3956D46DE8}\") = \"folder\", \"folder\", \"{22222222-2222-2222-2222-222222222222}\"",
            "	ProjectSection(SolutionItems) = preProject",
            "		README.md = README.md",
            "	EndProjectSection",
            "EndProject",
            "	GlobalSection(SolutionItems) = preProject",
            "		notes.txt = notes.txt",
            "	EndGlobalSection",
            "GlobalSection(NestedProjects) = preSolution",
            "	{11111111-1111-1111-1111-111111111111} = {22222222-2222-2222-2222-222222222222}",
            "EndGlobalSection");

        var adapted = WorkspaceLoad.AdaptFilteredSolutionForStaging(sln, _root);
        var lines = adapted.Split("\r\n");

        Assert.Contains(
            Path.GetFullPath(Path.Combine(_root, "src\\Core\\Core.csproj")),
            lines.Single(l => l.StartsWith("Project(", StringComparison.Ordinal) && l.Contains("Core.csproj", StringComparison.Ordinal)));
        // SolutionItems blocks vanish entirely — folder section included;
        // NestedProjects (GUID-only) survives.
        Assert.DoesNotContain(lines, l => l.Contains("SolutionItems", StringComparison.Ordinal));
        Assert.DoesNotContain(lines, l => l.Contains("README.md", StringComparison.Ordinal));
        Assert.Contains(lines, l => l.Contains("NestedProjects", StringComparison.Ordinal));
        // The solution-folder entry keeps its extension-less folder name.
        Assert.Contains(
            " = \"folder\", \"folder\",", lines.Single(l => l.Contains("2150E333", StringComparison.Ordinal)));
    }
}
