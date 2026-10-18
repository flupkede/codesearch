using System.Diagnostics;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace ScipCsharp;

/// <summary>Process exit codes beyond the generic 0/1.</summary>
internal static class ExitCodes
{
    /// <summary>
    /// A <c>dotnet restore</c> for this solution is still running (or its
    /// marker says so): no index was built and no output file was written.
    /// The Rust host maps this to a restore-pending outcome, never a generic
    /// error (issue #225).
    /// </summary>
    public const int RestorePending = 75;
}

/// <summary>
/// Thrown when a gated command (index, find-refs, batch-find-refs) must not
/// load the workspace because a restore is in flight — building a degraded
/// index now would store CS0246 noise into LMDB while the previous good index
/// could have stayed live.
/// </summary>
internal sealed class RestorePendingException : Exception
{
    public RestorePendingException()
        : base("restore pending — workspace load refused (issue #225)")
    {
    }
}

/// <summary>
/// The gate decision for one workspace load: an ACTIVE restore marker for
/// this solution refuses the load with
/// <see cref="RestorePendingException"/> — a degraded index built while
/// restore is in flight would poison LMDB while the previous good index
/// could have stayed live. A stale marker is deleted by the liveness check
/// and the load proceeds. Serve mode never calls this: a resident process
/// must keep answering from its existing workspace.
/// </summary>
internal static class RestoreGate
{
    public static void ThrowIfRestorePending(string solutionPath)
    {
        if (RestorePendingMarker.IsActive(solutionPath))
        {
            Console.Error.WriteLine(
                "[RESTORE-PENDING] a dotnet restore for this solution is already running — " +
                $"exiting {ExitCodes.RestorePending} without building an index; retry when it finishes.");
            throw new RestorePendingException();
        }
    }

    /// <summary>
    /// Serve-startup refusal: returns
    /// <see cref="ExitCodes.RestorePending"/> when a restore is in flight, so
    /// the resident pool falls back to the one-shot path — a workspace loaded
    /// mid-restore would have its find-refs answers CACHED by the host. Null
    /// when the load may proceed. Exit-code form instead of throwing: a
    /// resident process reports refusal as its exit status.
    /// </summary>
    public static int? ServeStartupPendingExit(string solutionPath)
    {
        if (RestorePendingMarker.IsActive(solutionPath))
        {
            Console.Error.WriteLine(
                "[RESTORE-PENDING] a dotnet restore for this solution is already running — " +
                "serve refuses to load the workspace; the pool will retry once it finishes.");
            return ExitCodes.RestorePending;
        }
        return null;
    }
}

/// <summary>
/// Marker outside the repo recording a still-running <c>dotnet restore</c>:
/// <c>%TEMP%/codesearch-restore/&lt;sha256(solution path)&gt;.json</c>. A
/// later gated run that sees an ACTIVE marker (pid alive with the recorded
/// start time) exits <see cref="ExitCodes.RestorePending"/> immediately
/// instead of starting a second restore; a stale one (restore finished or
/// died — pid reuse is caught by the start-time match) is deleted and the run
/// proceeds normally.
/// </summary>
internal static class RestorePendingMarker
{
    private static string MarkerDirectory =>
        Path.Combine(Path.GetTempPath(), "codesearch-restore");

    private static string MarkerPath(string solutionPath)
    {
        // Upper-cased so the same solution reached through different drive
        // casing maps to one marker on case-insensitive filesystems.
        var normalized = Path.GetFullPath(solutionPath).ToUpperInvariant();
        var hash = SHA256.HashData(Encoding.UTF8.GetBytes(normalized));
        return Path.Combine(MarkerDirectory, Convert.ToHexString(hash) + ".json");
    }

    public static void Write(string solutionPath, int pid, DateTime startedUtc)
    {
        Directory.CreateDirectory(MarkerDirectory);
        var payload = JsonSerializer.Serialize(new MarkerPayload(pid, startedUtc));
        File.WriteAllText(MarkerPath(solutionPath), payload);
    }

    /// <summary>
    /// True when the recorded restore process is still alive with the
    /// recorded start time. Any other outcome (missing, corrupt, dead pid,
    /// start-time mismatch) deletes the marker and returns false.
    /// </summary>
    public static bool IsActive(string solutionPath)
    {
        var path = MarkerPath(solutionPath);
        try
        {
            if (!File.Exists(path))
            {
                return false;
            }
            var payload = JsonSerializer.Deserialize<MarkerPayload>(File.ReadAllText(path));
            if (payload is not null && IsProcessAlive(payload.Pid, payload.StartedUtc))
            {
                return true;
            }
        }
        catch (Exception ex)
        {
            Console.Error.WriteLine(
                $"[INFO] restore marker unreadable ({ex.GetType().Name}: {ex.Message}) — treating as stale.");
        }
        TryDelete(path);
        return false;
    }

    public static void Delete(string solutionPath) => TryDelete(MarkerPath(solutionPath));

    private static void TryDelete(string path)
    {
        try
        {
            File.Delete(path);
        }
        catch
        {
            // Best effort: a marker that survives cleanup only costs one
            // extra restore-pending exit on the next run, which re-checks.
        }
    }

    private static bool IsProcessAlive(int pid, DateTime startedUtc)
    {
        try
        {
            using var process = Process.GetProcessById(pid);
            // Tolerance absorbs StartTime precision differences across
            // platforms (/proc-based times are second-granular on Linux).
            return Math.Abs((process.StartTime.ToUniversalTime() - startedUtc).TotalSeconds) < 5;
        }
        catch (Exception ex) when (
            ex is ArgumentException or InvalidOperationException
            or System.ComponentModel.Win32Exception)
        {
            return false;
        }
    }

    private sealed record MarkerPayload(int Pid, DateTime StartedUtc);
}
