namespace ScipCsharp;

/// <summary>
/// One installable MSBuild candidate for the capability-aware instance
/// choice: either the MSBuild of a .NET SDK install (CLI-visible) or of a
/// Visual Studio instance (bundled with VS, independent of
/// <c>C:\Program Files\dotnet</c>). Dependency-free so the decision logic
/// is unit-testable without spinning up real installs.
/// </summary>
internal readonly record struct MsbuildCandidate(Version Version, bool IsDotNetSdk);

internal static class MsbuildRestore
{
    /// <summary>
    /// vswhere ships in the fixed VS Installer location on every Windows
    /// machine; probing it anywhere else just multiplies failure modes.
    /// </summary>
    private const string VsWherePath = @"C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe";

    // NETSDK1045: "The current .NET SDK does not support targeting .NET X.0".
    private const string SdkDoesNotSupportTargeting = "does not support targeting";
    // Current hosts: "A compatible .NET SDK was not found" (global.json
    // unsatisfiable); legacy hosts worded it "A compatible installed .NET
    // SDK for global.json at ... was not found." Match both — neither
    // phrasing contains the other.
    private const string SdkGlobalJsonNotFoundModern = "compatible .NET SDK";
    private const string SdkGlobalJsonNotFoundLegacy = "compatible installed .NET SDK";

    /// <summary>vswhere probe bound — generous for a local EXE, still bounded.</summary>
    private const int VsWhereWaitMs = 10_000;

    /// <summary>
    /// True when a <c>dotnet restore</c> failed on SDK resolution — the
    /// CLI-visible SDK set cannot target the repo's TFM (NETSDK1045) or
    /// cannot satisfy global.json — rather than on packages. These are the
    /// failures a Visual Studio install can still recover: VS ships its own
    /// MSBuild + SDK targets, blind spots of <c>C:\Program Files\dotnet</c>
    /// included (2026-10-15 incident: a hollow 10.0.101 SDK dir left the
    /// CLI unable to target net10 while VS built fine). Feed/auth failures
    /// (NU11xx) deliberately do NOT match — VS restores through the same
    /// NuGet stack and would fail identically, so retrying those buys a
    /// second slow failure.
    /// </summary>
    internal static bool IsSdkResolutionFailure(string restoreOutput) =>
        restoreOutput.Contains(SdkDoesNotSupportTargeting, StringComparison.OrdinalIgnoreCase)
        || restoreOutput.Contains(SdkGlobalJsonNotFoundModern, StringComparison.OrdinalIgnoreCase)
        || restoreOutput.Contains(SdkGlobalJsonNotFoundLegacy, StringComparison.OrdinalIgnoreCase);

    /// <summary>
    /// Full path of the newest VS MSBuild via vswhere, or null when vswhere,
    /// VS, or the MSBuild component is absent (non-Windows included).
    /// Best effort by design: a probe failure must never break the restore
    /// path it is meant to improve. <c>-prerelease</c> includes Insiders
    /// channels; <c>-find</c> (vswhere ≥ 2.8) returns MSBuild.exe paths
    /// directly, newest instance only via <c>-latest</c>.
    /// </summary>
    internal static string? LocateVsMsbuild()
    {
        if (!OperatingSystem.IsWindows() || !File.Exists(VsWherePath))
        {
            return null;
        }
        try
        {
            var psi = new System.Diagnostics.ProcessStartInfo
            {
                FileName = VsWherePath,
                Arguments = "-latest -prerelease -products * -requires Microsoft.Component.MSBuild " +
                    "-find MSBuild\\**\\Bin\\MSBuild.exe",
                RedirectStandardOutput = true,
                RedirectStandardError = true,
                UseShellExecute = false,
            };
            using var process = System.Diagnostics.Process.Start(psi);
            if (process is null)
            {
                return null;
            }
            // Drain async, THEN bounded-wait — and KILL on timeout: unlike a
            // restore (never killed — its work must survive), vswhere is a
            // local metadata probe whose death costs one failed probe. The
            // kill also closes the pipes, so the stdout read cannot block.
            var stdoutTask = process.StandardOutput.ReadToEndAsync();
            _ = process.StandardError.ReadToEndAsync();
            if (!process.WaitForExit(VsWhereWaitMs))
            {
                try
                {
                    process.Kill(entireProcessTree: true);
                }
                catch
                {
                    // Already exiting — nothing to bound anymore.
                }
                return null;
            }
            var path = ParseFirstVswherePath(stdoutTask.GetAwaiter().GetResult());
            return path is not null && File.Exists(path) ? path : null;
        }
        catch
        {
            return null;
        }
    }

    /// <summary>First non-blank line of vswhere -find output.</summary>
    internal static string? ParseFirstVswherePath(string vswhereOutput)
    {
        foreach (var raw in vswhereOutput.Split('\n'))
        {
            var line = raw.Trim().TrimEnd('\r');
            if (line.Length > 0)
            {
                return line;
            }
        }
        return null;
    }

    /// <summary>
    /// The highest .NET major the solution's projects provably target:
    /// max over every .csproj's first <c>TargetFramework(s)</c> raw-text
    /// match (raw text — this runs BEFORE any MSBuild evaluation, which is
    /// the whole point) and over global.json's pinned SDK version when one
    /// sits at or above the solution dir. null when nothing parseable was
    /// found — callers must then keep the default instance choice, because
    /// an unknown floor proves nothing.
    /// </summary>
    internal static int? RequiredSdkMajorFloor(string? solutionPath)
    {
        if (solutionPath is null || !File.Exists(solutionPath))
        {
            return null;
        }
        var solutionDir = Path.GetDirectoryName(Path.GetFullPath(solutionPath)) ?? ".";
        int? floor = null;
        foreach (var projectPath in EnumerateSolutionProjects(solutionPath, solutionDir))
        {
            var major = TargetFrameworkMajor(ReadTextBestEffort(projectPath));
            if (major is not null && (floor is null || major > floor))
            {
                floor = major;
            }
        }
        var globalJsonMajor = GlobalJsonSdkMajor(solutionDir);
        if (globalJsonMajor is not null && (floor is null || globalJsonMajor > floor))
        {
            floor = globalJsonMajor;
        }
        return floor;
    }

    /// <summary>
    /// Which install to register MSBuild from: the highest .NET SDK instance
    /// that can target <paramref name="requiredMajor"/> (matching the
    /// "register the highest available MSBuild instance" intent, unlike the
    /// historical first-match), falling back to the highest VS instance when
    /// no SDK instance qualifies — VS bundles its own SDK targets, so it can
    /// evaluate TFMs the installed CLI SDKs cannot. The VS path is the
    /// documented net472-BuildHost-crash risk, accepted ONLY when the SDK
    /// path is certain to fail the load anyway. Returns the index of the
    /// chosen candidate within <paramref name="candidates"/> and whether it
    /// is an SDK instance (for logging the flip explicitly).
    /// </summary>
    internal static (int Index, bool IsSdk) ChooseMsBuildInstance(
        IReadOnlyList<MsbuildCandidate> candidates,
        int? requiredMajor)
    {
        var sdkCandidates = candidates
            .Select((c, i) => (Candidate: c, Index: i))
            .Where(e => e.Candidate.IsDotNetSdk)
            .OrderByDescending(e => e.Candidate.Version)
            .ToList();
        if (sdkCandidates.Count > 0)
        {
            var qualifying = requiredMajor is null
                ? sdkCandidates[0]
                : sdkCandidates.FirstOrDefault(e => e.Candidate.Version.Major >= requiredMajor);
            if (!qualifying.Equals(default))
            {
                return (qualifying.Index, true);
            }
        }
        // No SDK instance can target the repo (or none exists): highest
        // instance overall — a VS install when present, else whatever the
        // SDK set offered (preserves the old behaviour as the last resort).
        var overall = candidates
            .Select((c, i) => (Candidate: c, Index: i))
            .OrderByDescending(e => e.Candidate.Version)
            .First();
        return (overall.Index, overall.Candidate.IsDotNetSdk);
    }

    private static IEnumerable<string> EnumerateSolutionProjects(string solutionPath, string solutionDir)
    {
        string[] lines;
        try
        {
            lines = File.ReadAllLines(solutionPath);
        }
        catch
        {
            yield break;
        }
        // Project("{GUID}") = "Name", "Relative\Path.csproj", "{GUID}"
        foreach (var line in lines)
        {
            var trimmed = line.Trim();
            if (!trimmed.StartsWith("Project(", StringComparison.Ordinal))
            {
                continue;
            }
            var parts = trimmed.Split(',');
            if (parts.Length < 2)
            {
                continue;
            }
            var pathPart = parts[1].Trim();
            if (pathPart.Length < 2 || !pathPart.StartsWith('"') || !pathPart.EndsWith('"'))
            {
                continue;
            }
            var relative = pathPart[1..^1];
            if (relative.EndsWith(".csproj", StringComparison.OrdinalIgnoreCase))
            {
                yield return Path.GetFullPath(
                    relative.Replace('\\', Path.DirectorySeparatorChar), Path.GetFullPath(solutionDir));
            }
        }
    }

    private static string? ReadTextBestEffort(string path)
    {
        try
        {
            return File.Exists(path) ? File.ReadAllText(path) : null;
        }
        catch
        {
            return null;
        }
    }

    /// <summary>
    /// The HIGHEST netX.Y major in raw project text — a floor wants the max,
    /// so a <c>net8.0;net10.0</c> multi-target floors at 10 (unlike the EOL
    /// skip-guard, whose first-TFM stance serves a different purpose).
    /// netstandard/netcoreapp deliberately do not match — they load under
    /// any modern SDK, and EOL ones are skipped anyway. The minor dot is
    /// REQUIRED: the extension-less .NET Framework spellings (net48,
    /// net472) must not raise the floor — an SDK loads those from reference
    /// assemblies, and a "48" floor would flip healthy SDK+VS machines to
    /// the VS instance for no reason.
    /// </summary>
    private static int? TargetFrameworkMajor(string? projectText)
    {
        if (projectText is null)
        {
            return null;
        }
        var match = System.Text.RegularExpressions.Regex.Match(
            projectText, @"<TargetFrameworks?>([^<]+)</");
        if (!match.Success)
        {
            return null;
        }
        int? floor = null;
        foreach (var raw in match.Groups[1].Value.Split(';'))
        {
            var version = System.Text.RegularExpressions.Regex.Match(raw.Trim(), @"^net(\d+)\.");
            if (version.Success && int.TryParse(version.Groups[1].Value, out var major)
                && (floor is null || major > floor))
            {
                floor = major;
            }
        }
        return floor;
    }

    /// <summary>
    /// Major of the SDK version pinned by the nearest global.json walking
    /// up from the solution dir — NuGet's own lookup order. rollForward
    /// never lowers the floor, so the pinned major is the safe bound.
    /// </summary>
    private static int? GlobalJsonSdkMajor(string solutionDir)
    {
        var dir = Path.GetFullPath(solutionDir);
        while (true)
        {
            var globalJson = Path.Combine(dir, "global.json");
            if (File.Exists(globalJson))
            {
                var match = System.Text.RegularExpressions.Regex.Match(
                    ReadTextBestEffort(globalJson) ?? "", @"""version""\s*:\s*""(\d+)\.");
                return match.Success && int.TryParse(match.Groups[1].Value, out var major) ? major : null;
            }
            var parent = Directory.GetParent(dir)?.FullName;
            if (string.IsNullOrEmpty(parent) || string.Equals(parent, dir, StringComparison.OrdinalIgnoreCase))
            {
                return null;
            }
            dir = parent;
        }
    }
}
