using System.Diagnostics;
using ScipCsharp;
using Xunit;

namespace ScipCsharp.Tests;

/// <summary>
/// Unit tests for the restore-pending marker: a marker for a live process
/// with matching start time is ACTIVE (gating later index/find-refs runs),
/// while a start-time mismatch or an unreachable pid is STALE and gets
/// deleted so the run proceeds normally.
/// </summary>
public class RestoreGateTests : IDisposable
{
    private readonly string _solutionPath;

    public RestoreGateTests()
    {
        _solutionPath = Path.Combine(
            Path.GetTempPath(), "cs-restore-gate-" + Guid.NewGuid().ToString("N"), "Acme.sln");
    }

    public void Dispose() => RestorePendingMarker.Delete(_solutionPath);

    [Fact]
    public void RestorePendingExitCodeIsTheDedicated75()
    {
        Assert.Equal(75, ExitCodes.RestorePending);
    }

    [Fact]
    public void MarkerForLiveProcessWithMatchingStartTimeIsActive()
    {
        using var current = Process.GetCurrentProcess();
        RestorePendingMarker.Write(_solutionPath, current.Id, current.StartTime.ToUniversalTime());

        Assert.True(RestorePendingMarker.IsActive(_solutionPath));
    }

    [Fact]
    public void MarkerWithMismatchedStartTimeIsStaleAndDeleted()
    {
        // Pid-reuse protection: a recycled pid with a different start time
        // must never look like a still-running restore.
        using var current = Process.GetCurrentProcess();
        RestorePendingMarker.Write(
            _solutionPath, current.Id, current.StartTime.ToUniversalTime().AddDays(-1));

        Assert.False(RestorePendingMarker.IsActive(_solutionPath));
        // Stale marker was deleted: a repeat check must not resurrect it.
        Assert.False(RestorePendingMarker.IsActive(_solutionPath));
    }

    [Fact]
    public void MarkerForUnreachablePidIsStale()
    {
        // Pid 4 exists on Windows (System) but denies StartTime, and does not
        // exist on Linux — both classification paths must end stale.
        RestorePendingMarker.Write(_solutionPath, 4, DateTime.UtcNow);

        Assert.False(RestorePendingMarker.IsActive(_solutionPath));
    }

    [Fact]
    public void MissingMarkerIsNeverActive()
    {
        Assert.False(RestorePendingMarker.IsActive(_solutionPath));
    }

    [Fact]
    public void MarkerIsSharedAcrossPathCasing()
    {
        using var current = Process.GetCurrentProcess();
        RestorePendingMarker.Write(_solutionPath, current.Id, current.StartTime.ToUniversalTime());

        Assert.True(RestorePendingMarker.IsActive(_solutionPath.ToUpperInvariant()));
    }

    /// <summary>
    /// Pins the gate decision itself: an ACTIVE marker must refuse the
    /// workspace load — deleting the Program.cs wiring must not let a
    /// degraded index be built silently.
    /// </summary>
    [Fact]
    public void ActiveMarkerRefusesTheWorkspaceLoad()
    {
        using var current = Process.GetCurrentProcess();
        RestorePendingMarker.Write(_solutionPath, current.Id, current.StartTime.ToUniversalTime());

        Assert.Throws<RestorePendingException>(() => RestoreGate.ThrowIfRestorePending(_solutionPath));
    }

    [Fact]
    public void MissingOrStaleMarkerAllowsTheWorkspaceLoad()
    {
        RestoreGate.ThrowIfRestorePending(_solutionPath); // no marker: no throw

        using var current = Process.GetCurrentProcess();
        RestorePendingMarker.Write(
            _solutionPath, current.Id, current.StartTime.ToUniversalTime().AddDays(-1));

        RestoreGate.ThrowIfRestorePending(_solutionPath); // stale: deleted, no throw
    }

    /// <summary>
    /// Pins the serve-startup refusal: an ACTIVE marker must yield the
    /// dedicated exit code so the resident pool falls back instead of caching
    /// answers from a workspace loaded mid-restore.
    /// </summary>
    [Fact]
    public void ServeStartupRefusesWhileRestoreInFlight()
    {
        using var current = Process.GetCurrentProcess();
        RestorePendingMarker.Write(_solutionPath, current.Id, current.StartTime.ToUniversalTime());

        Assert.Equal(75, RestoreGate.ServeStartupPendingExit(_solutionPath));
    }

    [Fact]
    public void ServeStartupProceedsWithoutActiveMarker()
    {
        Assert.Null(RestoreGate.ServeStartupPendingExit(_solutionPath));
    }
}
