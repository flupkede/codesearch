namespace ScipCsharp;

/// <summary>
/// Global MSBuild properties for every workspace load. Non-negotiables:
/// DesignTimeBuild + SkipCompilerExecution and NO warnings-as-errors — a
/// repo that escalates warnings (own settings or Directory.Build.props)
/// would turn benign load-time diagnostics (the NU1603 version-unification
/// class) into MSBuild project-load failures, dropping the project — and
/// every cross-project reference to it — from the index. Everything else
/// about the project keeps its own settings; we only refuse the escalation.
/// Lives outside Program.cs so the test assembly can compile it in
/// (Program carries the Main entrypoint and is deliberately excluded there).
/// </summary>
internal static class WorkspaceLoad
{
    internal static System.Collections.Generic.Dictionary<string, string> GlobalProperties() => new()
    {
        // Tell Roslyn to skip projects it cannot load instead of crashing.
        { "BuildingInsideVisualStudio", "true" },
        // Design-time build: prevents auto-generated files in obj/ (e.g.
        // .AssemblyAttributes.cs, .AssemblyInfo.cs) from being included as
        // explicit Compile items. Without this, SDK-style projects produce
        // duplicate Compile items (auto-include + obj/ generated), which
        // causes MSBuildWorkspace to fail loading the project.
        { "DesignTimeBuild", "true" },
        { "SkipCompilerExecution", "true" },
        // Warnings stay warnings during the load — never fatal (NU1603 etc.).
        { "TreatWarningsAsErrors", "false" },
        { "WarningsAsErrors", "" },
        { "MSBuildTreatWarningsAsErrors", "false" },
    };

    /// <summary>
    /// Delete stale filtered-solution leftovers (`<c>base-filtered-*.sln</c>`,
    /// case-insensitive) from earlier runs whose process died before the
    /// best-effort finally-delete in Program.OpenSolutionFilteredAsync.
    /// Hygiene only — the watcher never sees them — so every failure is
    /// swallowed; returns the deleted count. Lives here, not in Program.cs,
    /// so the test assembly can compile it in.
    /// </summary>
    internal static int CleanupStaleFilteredSolutions(string solutionDir, string solutionBaseName)
    {
        try
        {
            var prefix = solutionBaseName + "-filtered-";
            var stale = System.IO.Directory
                .EnumerateFiles(solutionDir, "*.sln")
                .Where(f => System.IO.Path.GetFileName(f)
                    .StartsWith(prefix, StringComparison.OrdinalIgnoreCase))
                .ToList();
            var deleted = 0;
            foreach (var path in stale)
            {
                try
                {
                    System.IO.File.Delete(path);
                    deleted++;
                }
                catch (Exception)
                {
                    // Locked or vanished — the next run retries.
                }
            }
            if (deleted > 0)
            {
                Console.Error.WriteLine(
                    $"[INFO] Deleted {deleted} stale filtered solution(s) left by a previous run");
            }
            return deleted;
        }
        catch (Exception)
        {
            // Scanning must never break the workspace load.
            return 0;
        }
    }
}
