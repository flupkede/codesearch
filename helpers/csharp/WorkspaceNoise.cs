namespace ScipCsharp;

using Microsoft.CodeAnalysis;

/// <summary>
/// Classifies workspace-load noise that must not surface as index warnings.
/// Compiled into both scip-csharp and the test project via a linked Compile
/// item (same pattern as WorkspaceLoad.cs).
/// </summary>
internal static class WorkspaceNoise
{
    /// <summary>
    /// The demoted emission for a failure-kind diagnostic that is noise, not
    /// index degradation. Renders <see cref="WorkspaceDiagnostic.Message"/>
    /// — never the diagnostic itself: its <c>ToString()</c> prefixes
    /// "[Failure] ", and the Rust capture keys on a substring match for
    /// "[Failure]" anywhere in the line, which would pull the demoted note
    /// right back into the stored index warnings.
    /// </summary>
    public static string DemotedNote(WorkspaceDiagnostic diagnostic) =>
        $"[INFO] Workspace note: {diagnostic.Message}";
    /// <summary>
    /// True when a workspace-failure diagnostic is a NuGet audit advisory
    /// ("Package 'X' has a known high severity vulnerability, ...") rather
    /// than a load failure — emitting it would pin the repo's index warning
    /// until the package is bumped, while the index itself is unaffected.
    /// Pure and null-tolerant so it is unit-testable.
    /// </summary>
    public static bool IsNuGetAuditNoise(string? message) =>
        message is not null
        && message.Contains("has a known", StringComparison.OrdinalIgnoreCase)
        && message.Contains("vulnerability", StringComparison.OrdinalIgnoreCase);

    /// <summary>
    /// True when a workspace-failure diagnostic is a NuGet package-version
    /// mismatch ("Dependency specified was 'X (&gt;= A)' but ended up with
    /// 'X B'"): the affected project still loads and its symbol tree is
    /// complete, so nothing here means "find-impact may miss symbols" — the
    /// build owner settles the pin, and the index must not carry the noise.
    /// Pure and null-tolerant so it is unit-testable.
    /// </summary>
    public static bool IsPackageVersionConflict(string? message) =>
        message is not null
        && message.Contains("Dependency specified was", StringComparison.OrdinalIgnoreCase)
        && message.Contains("ended up with", StringComparison.OrdinalIgnoreCase);

    /// <summary>
    /// True when a workspace-failure diagnostic is an MSBuild check advisory
    /// escalated to a "failure" (observed live: "Grpc.Net.ClientFactory
    /// 2.63.0 or earlier could cause issues when used together with
    /// Microsoft.Extensions.Http.Resilience. ... Consider using
    /// Grpc.Net.ClientFactory 2.64.0 or later. To suppress the warning set
    /// SuppressCheckGrpcNetClientFactoryVersion=true."). A package
    /// compatibility recommendation for the product, not a load failure:
    /// the project still loads and the index is unaffected. Pure and
    /// null-tolerant so it is unit-testable.
    /// </summary>
    public static bool IsMsbuildCheckAdvisory(string? message)
    {
        if (message is null)
        {
            return false;
        }
        return message.Contains("could cause issues when used together with", StringComparison.OrdinalIgnoreCase)
            || (message.Contains("To suppress the warning set", StringComparison.OrdinalIgnoreCase)
                && message.Contains("Consider using", StringComparison.OrdinalIgnoreCase));
    }

    /// <summary>
    /// True when a workspace-failure diagnostic is BuildHost crash AFTERMATH
    /// (observed live: "[BuildHost PID 58740] Exception while shutting down
    /// the BuildHost process." / "[BuildHost PID 58740] The BuildHost
    /// process is not responding. Process output:"). These lines trail the
    /// real failure, which surfaces separately (the load exception itself);
    /// carrying them as warnings doubles the failure count without adding a
    /// cause. Pure and null-tolerant so it is unit-testable.
    /// </summary>
    public static bool IsBuildHostShutdownChatter(string? message)
    {
        if (message is null)
        {
            return false;
        }
        return message.Contains("[BuildHost PID", StringComparison.OrdinalIgnoreCase)
            && (message.Contains("Exception while shutting down", StringComparison.OrdinalIgnoreCase)
                || message.Contains("not responding", StringComparison.OrdinalIgnoreCase));
    }

    /// <summary>
    /// True when a restore or load diagnostic is a NuGet source that cannot
    /// be reached or authenticated ("Unable to load the service index for
    /// source ..."). A host environment problem, not a code defect; the
    /// restore fallback already reports the affected projects. "Unable to
    /// find package" is deliberately not matched: it means a package is
    /// really missing and the project loads degraded. Pure and null-tolerant.
    /// </summary>
    public static bool IsNuGetSourceFailure(string? message)
    {
        if (message is null)
        {
            return false;
        }
        return message.Contains("Unable to load the service index for source", StringComparison.OrdinalIgnoreCase)
            || message.Contains("Error occurred while getting package vulnerability data", StringComparison.OrdinalIgnoreCase);
    }

    /// <summary>
    /// True when a load failure is the MSBuild BuildHost's type initializer
    /// crashing on <c>Microsoft.Build.Shared.XMakeElements</c>. The affected
    /// project is still attempted by the per-project fallback; the crash is
    /// tool-side and must not pin the index warning. Pure and null-tolerant.
    /// </summary>
    public static bool IsBuildHostTypeInitializerCrash(string? message) =>
        message is not null
        && message.Contains("XMakeElements", StringComparison.OrdinalIgnoreCase);
}
