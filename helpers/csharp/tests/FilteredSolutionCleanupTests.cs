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
    }
}
