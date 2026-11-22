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
    /// Staging directory for the transient filtered solution the workspace
    /// load writes before opening it: `<c>.codesearch.db\tmp</c>` under the
    /// solution dir. Inside the repo (same volume, AV-safe — %TEMP% is what
    /// virus scanners latch onto) yet invisible to the Rust watcher, walker
    /// and bootstrap scan, which all hard-exclude the `.codesearch.db` name.
    /// Created on demand; marked hidden on Windows to match what the
    /// indexer does with the database dir.
    /// </summary>
    internal static string FilteredSolutionStagingDir(string solutionDir)
    {
        var dbDir = System.IO.Path.Combine(solutionDir, ".codesearch.db");
        var dir = System.IO.Path.Combine(dbDir, "tmp");
        System.IO.Directory.CreateDirectory(dir);
        TryHide(dbDir);
        return dir;
    }

    private static void TryHide(string path)
    {
        if (!OperatingSystem.IsWindows()) return;
        try
        {
            var attrs = System.IO.File.GetAttributes(path);
            if ((attrs & System.IO.FileAttributes.Hidden) == 0)
                System.IO.File.SetAttributes(path, attrs | System.IO.FileAttributes.Hidden);
        }
        catch (Exception)
        {
            // Cosmetic — never fail a load over a hidden bit.
        }
    }

    /// <summary>
    /// Adapts filtered-solution content for the staging location under
    /// <c>.codesearch.db/tmp</c>: project paths become absolute (the staged
    /// file sits deeper than the solution it mirrors, so relative paths
    /// would dangle) and SolutionItems sections are dropped — IDE cosmetics
    /// the design-time build never reads, whose relative paths would dangle
    /// too.
    /// </summary>
    internal static string AdaptFilteredSolutionForStaging(string slnContent, string solutionDir)
    {
        var sourceLines = slnContent.Split('\n');
        var result = new List<string>(sourceLines.Length);
        var inSolutionItems = false;
        foreach (var raw in sourceLines)
        {
            var line = raw.TrimEnd('\r');
            var trimmed = line.Trim();
            if (inSolutionItems)
            {
                if (trimmed.StartsWith("EndGlobalSection", StringComparison.Ordinal) ||
                    trimmed.StartsWith("EndProjectSection", StringComparison.Ordinal))
                {
                    inSolutionItems = false;
                }
                continue;
            }
            if (trimmed.StartsWith("GlobalSection(SolutionItems", StringComparison.Ordinal) ||
                trimmed.StartsWith("ProjectSection(SolutionItems", StringComparison.Ordinal))
            {
                inSolutionItems = true;
                continue;
            }
            if (trimmed.StartsWith("Project(", StringComparison.Ordinal))
            {
                result.Add(AbsoluteProjectLine(line, solutionDir));
                continue;
            }
            result.Add(line);
        }
        // A .sln is a CRLF format: join with "\r\n" regardless of platform so
        // the staged file's bytes (and the parity test that splits on "\r\n")
        // are identical on Windows and Linux.
        return string.Join("\r\n", result);
    }

    /// <summary>
    /// Rewrites the path segment of one Project() line to an absolute path.
    /// Quoted spans on such a line are TypeGuid, Name, Path, Guid — the path
    /// is the third. Solution-folder entries carry a folder name without a
    /// file extension and stay untouched, as do already-rooted paths.
    /// </summary>
    private static string AbsoluteProjectLine(string line, string solutionDir)
    {
        var spans = new List<(int Start, int End)>();
        int start = -1;
        for (int i = 0; i < line.Length; i++)
        {
            if (line[i] != '"') continue;
            if (start < 0) start = i + 1;
            else
            {
                spans.Add((start, i));
                start = -1;
            }
        }
        if (spans.Count < 3) return line;
        var (s, e) = spans[2];
        var relative = line[s..e];
        if (!System.IO.Path.HasExtension(relative) || System.IO.Path.IsPathRooted(relative)) return line;
        var absolute = System.IO.Path.GetFullPath(System.IO.Path.Combine(solutionDir, relative));
        return line[..s] + absolute + line[e..];
    }

    /// <summary>
    /// Delete stale filtered-solution leftovers (`<c>base-filtered-*.sln</c>`,
    /// case-insensitive) from earlier runs whose process died before the
    /// best-effort finally-delete in Program.OpenSolutionFilteredAsync.
    /// Sweeps the staging dir (`.codesearch.db/tmp`) and the solution root
    /// (leftovers from before filtered solutions moved into the database
    /// dir). Hygiene only — every failure is swallowed; returns the deleted
    /// count. Lives here, not in Program.cs, so the test assembly can
    /// compile it in.
    /// </summary>
    internal static int CleanupStaleFilteredSolutions(string solutionDir, string solutionBaseName)
    {
        try
        {
            var prefix = solutionBaseName + "-filtered-";
            // The staging dir is scanned without creating it — creation is
            // the writer's job (FilteredSolutionStagingDir), never a side
            // effect of a cleanup scan over a nonexistent solution dir.
            var staging = System.IO.Path.Combine(solutionDir, ".codesearch.db", "tmp");
            var stale = new[] { solutionDir, staging }
                .SelectMany(dir => SafeSlnFiles(dir))
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

    private static IEnumerable<string> SafeSlnFiles(string dir)
    {
        if (!System.IO.Directory.Exists(dir)) yield break;
        foreach (var f in System.IO.Directory.EnumerateFiles(dir, "*.sln")) yield return f;
    }
}
