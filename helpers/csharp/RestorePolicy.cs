using System.Diagnostics;

namespace ScipCsharp;

/// <summary>
/// Restore policy for the design-time workspace load: MSBuildWorkspace does
/// not restore, but running <c>dotnet restore</c> on every reindex is far too
/// heavy for multi-repo hubs. Restore only when the evidence says assets are
/// missing or stale. Compiled into both scip-csharp and the test project via
/// a linked Compile item (same pattern as WorkspaceLoad.cs).
/// </summary>
/// <summary>
/// File names whose change invalidates NuGet restore state even though no
/// .csproj changed: under central package management versions live in
/// Directory.Packages.props, and MSBuild/NuGet import the rest from parent
/// directories. Compared case-insensitively.
/// </summary>
public static class RestoreInputs
{
    public const string PackagesProps = "Directory.Packages.props";
    public const string BuildProps = "Directory.Build.props";
    public const string BuildTargets = "Directory.Build.targets";
    public const string NuGetConfig = "NuGet.config";
    public const string GlobalJson = "global.json";

    public static readonly string[] FileNames =
    {
        PackagesProps, BuildProps, BuildTargets, NuGetConfig, GlobalJson,
    };
}

/// <summary>
/// End-of-life <c>netcoreapp</c> frameworks: a modern SDK can still build
/// them, but the design-time workspace load cannot resolve their reference
/// assemblies, and every type use cascades into CS0518 ("System.Object is
/// not defined") — a tool-side gap, not a code-health signal. Projects
/// targeting one of these prefixes are skipped with an [INFO] note instead
/// of being compiled into warning noise. Compared case-insensitively;
/// <c>netstandard2.0</c> libraries are deliberately NOT skipped (legitimately
/// modern) and neither is anything on the supported <c>net</c> line.
/// </summary>
public static class SkippedFrameworks
{
    public static readonly string[] EndOfLifePrefixes =
    {
        "netcoreapp1", "netcoreapp2", "netcoreapp3",
    };

    public static bool IsEndOfLife(string targetFramework)
    {
        var tfm = targetFramework.Trim();
        foreach (var prefix in EndOfLifePrefixes)
        {
            if (tfm.StartsWith(prefix, StringComparison.OrdinalIgnoreCase))
            {
                return true;
            }
        }
        return false;
    }

    /// <summary>
    /// The first target framework from raw .csproj text — best effort:
    /// conditional property groups are not evaluated, which is fine for a
    /// skip-guard (a project whose FIRST target is EOL is exactly the one
    /// whose load would cascade).
    /// </summary>
    public static string? ExtractFromProjectText(string projectText)
    {
        var match = System.Text.RegularExpressions.Regex.Match(
            projectText, @"<TargetFrameworks?>([^<]+)</");
        if (!match.Success)
        {
            return null;
        }
        return match.Groups[1].Value.Split(';')[0].Trim();
    }
}

internal static class RestorePolicy
{
    /// <summary>Env override for the restore kill timeout, in seconds.</summary>
    public const string TimeoutEnvName = "CODESEARCH_RESTORE_TIMEOUT_SECS";
    private const string RestoreTimeoutEnv = TimeoutEnvName;
    private const int DefaultRestoreTimeoutSeconds = 300;

    /// <summary>
    /// True when at least one project lacks <c>obj/project.assets.json</c>, has
    /// one older than its .csproj, or has one older than ANY restore input
    /// (<see cref="RestoreInputs"/>) found walking up from the project dir to
    /// the solution dir and the repo root. False means every project has fresh
    /// assets and restore can be skipped entirely — the steady state of an
    /// actively developed repo.
    /// </summary>
    public static bool NeedsRestore(IEnumerable<string> projectRelativePaths, string solutionDir)
    {
        var walkRoot = FindWalkRoot(solutionDir);
        foreach (var relative in projectRelativePaths)
        {
            var projectPath = ResolveProjectPath(solutionDir, relative);
            var projectDir = Path.GetDirectoryName(projectPath) ?? solutionDir;
            var assetsPath = Path.Combine(projectDir, "obj", "project.assets.json");
            if (!File.Exists(assetsPath))
            {
                return true;
            }
            var assetsTime = File.GetLastWriteTimeUtc(assetsPath);
            if (File.GetLastWriteTimeUtc(projectPath) > assetsTime)
            {
                return true;
            }
            if (AnyRestoreInputNewerThan(projectDir, walkRoot, assetsTime))
            {
                return true;
            }
        }
        return false;
    }

    /// <summary>
    /// The highest directory restore inputs are collected from: the repo root
    /// when one sits at or above the solution dir (inputs above the solution
    /// dir still apply to it), otherwise the solution dir itself.
    /// </summary>
    private static string FindWalkRoot(string solutionDir)
    {
        var dir = Path.GetFullPath(solutionDir);
        while (true)
        {
            if (Directory.Exists(Path.Combine(dir, ".git")) || File.Exists(Path.Combine(dir, ".git")))
            {
                return dir;
            }
            var parent = Directory.GetParent(dir)?.FullName;
            if (string.IsNullOrEmpty(parent) || PathsEqual(parent, dir))
            {
                return Path.GetFullPath(solutionDir);
            }
            dir = parent;
        }
    }

    private static bool AnyRestoreInputNewerThan(string startDir, string walkRoot, DateTime assetsTime)
    {
        var dir = PathGuard.Inside(walkRoot, startDir);
        while (true)
        {
            // Enumerate + compare case-insensitively instead of File.Exists
            // probes: NuGet honours e.g. lowercase nuget.config on
            // case-sensitive filesystems where an exact-case probe misses it.
            foreach (var file in Directory.GetFiles(dir))
            {
                var fileName = Path.GetFileName(file);
                if (RestoreInputs.FileNames.Contains(fileName, StringComparer.OrdinalIgnoreCase)
                    && File.GetLastWriteTimeUtc(file) > assetsTime)
                {
                    return true;
                }
            }
            if (PathsEqual(dir, walkRoot))
            {
                return false;
            }
            var parent = Directory.GetParent(dir)?.FullName;
            if (string.IsNullOrEmpty(parent) || PathsEqual(parent, dir))
            {
                return false;
            }
            dir = parent;
        }
    }

    /// <summary>
    /// True when the project has an <c>obj/project.assets.json</c> at all:
    /// after a FAILED restore this distinguishes "restore-failed but usable
    /// existing assets" (index it; its compile errors are restore-caused)
    /// from "nothing to compile against" (exclude the project entirely).
    /// </summary>
    public static bool HasUsableAssets(string projectRelativePath, string solutionDir)
    {
        var projectPath = ResolveProjectPath(solutionDir, projectRelativePath);
        var projectDir = Path.GetDirectoryName(projectPath) ?? solutionDir;
        return File.Exists(Path.Combine(projectDir, "obj", "project.assets.json"));
    }

    // MSBuild/Windows paths use backslashes; on non-Windows '\' is an ordinary filename char.
    private static string ResolveProjectPath(string solutionDir, string relative) =>
        Path.GetFullPath(Path.Combine(solutionDir, relative.Replace('\\', Path.DirectorySeparatorChar)));

    private static bool PathsEqual(string left, string right) =>
        string.Equals(
            Path.GetFullPath(left).TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar),
            Path.GetFullPath(right).TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar),
            StringComparison.OrdinalIgnoreCase);

    /// <summary>
    /// The diagnostic kernel of a failed restore — up to three "error" lines
    /// (NU11xx feed/resolution failures tell the operator WHAT to fix, e.g.
    /// an unreachable private feed), each capped at 160 chars, joined for the
    /// single [WARN] line. The restore's own output is otherwise discarded.
    /// </summary>
    public static string ExtractRestoreErrors(string restoreOutput)
    {
        if (string.IsNullOrWhiteSpace(restoreOutput))
        {
            return string.Empty;
        }
        var errors = restoreOutput
            .Split('\n')
            .Select(l => l.TrimEnd('\r'))
            .Where(l => l.Contains("error", StringComparison.OrdinalIgnoreCase))
            .Select(l => l.Length > 160 ? l[..157] + "..." : l)
            .Take(3)
            .ToList();
        return errors.Count == 0
            ? string.Empty
            : " Restore output: " + string.Join(" | ", errors);
    }

    /// <summary>
    /// Restore wait timeout in seconds: <c>CODESEARCH_RESTORE_TIMEOUT_SECS</c>
    /// when set to a positive integer, otherwise 300. Unparseable values fall
    /// back to the default rather than failing the load.
    /// </summary>
    public static int RestoreTimeoutSeconds()
    {
        var raw = Environment.GetEnvironmentVariable(RestoreTimeoutEnv);
        if (int.TryParse(raw, out var seconds) && seconds > 0)
        {
            return seconds;
        }
        return DefaultRestoreTimeoutSeconds;
    }

    /// <summary>
    /// The line emitted when the bounded wait expires while a restore is
    /// still running — serve mode only: gated commands exit
    /// <see cref="ExitCodes.RestorePending"/> instead. The restore is NOT
    /// killed: it keeps running in the background (the child outlives this
    /// helper), serve keeps answering from the existing workspace, and any
    /// later index/find-refs run is gated by the restore marker until the
    /// fresh assets land. INFO, never WARN: restore state must not drive the
    /// caller-facing warning icon. Pinned by test so a "killed" wording
    /// cannot sneak back in. <paramref name="tool"/> names the restore
    /// flavor still running (dotnet restore, or the VS MSBuild fallback).
    /// </summary>
    public static string StillRunningWarning(int seconds, string tool = "dotnet restore") =>
        $"[INFO] {tool} still running after {seconds}s — serve keeps answering from the " +
        "existing workspace (degraded symbols) while restore continues in the background; " +
        "index and find-refs runs are gated until it finishes and the next reindex picks up " +
        $"the fresh assets. Raise with {TimeoutEnvName} if this solution legitimately needs longer.";
}
