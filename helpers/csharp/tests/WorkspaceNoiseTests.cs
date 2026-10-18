using ScipCsharp;
using Xunit;

namespace ScipCsharp.Tests;
/// <summary>
/// Unit tests for the workspace-noise policy: NuGet audit advisories must not
/// surface as index warnings (they are security notes, not load failures),
/// while genuine Msbuild load failures still do.
/// </summary>
public class WorkspaceNoiseTests
{
    [Fact]
    public void NuGetAuditAdvisoryIsFiltered()
    {
        var message =
            "Msbuild failed when processing the file 'Acme.Core.csproj' with message: " +
            "Package 'Microsoft.Extensions.Caching.Memory' 8.0.0 has a known high severity " +
            "vulnerability, https://github.com/advisories/GHSA-qj66-m88j-hmgj";
        Assert.True(WorkspaceNoise.IsNuGetAuditNoise(message));
    }

    [Theory]
    [InlineData("Acme.Core.csproj: Unable to load the service index for source https://pkgs.example/index.json.")]
    [InlineData("Acme.Core.csproj: Error occurred while getting package vulnerability data: Unable to load the service index")]
    public void NuGetSourceFailureIsDemoted(string message) =>
        Assert.True(WorkspaceNoise.IsNuGetSourceFailure(message));

    [Fact]
    public void MissingPackageIsNotNuGetSourceFailure() =>
        Assert.False(WorkspaceNoise.IsNuGetSourceFailure(
            "Acme.Core.csproj: Unable to find package Acme.Shared. No packages exist with this id in source(s)"));

    [Fact]
    public void GenuineLoadFailureIsNotNuGetSourceFailure() =>
        Assert.False(WorkspaceNoise.IsNuGetSourceFailure("The type or namespace name 'Catalog' does not exist"));

    [Fact]
    public void BuildHostTypeInitializerCrashIsDemoted() =>
        Assert.True(WorkspaceNoise.IsBuildHostTypeInitializerCrash(
            "RemoteInvocationException: An exception of type System.TypeInitializationException was thrown: The type initializer for 'Microsoft.Build.Shared.XMakeElements' threw an exception."));

    [Fact]
    public void GenuineLoadFailureIsNotTypeInitializerCrash() =>
        Assert.False(WorkspaceNoise.IsBuildHostTypeInitializerCrash("The type or namespace name 'Catalog' does not exist"));

    [Fact]
    public void GenuineLoadFailureIsKept()
    {
        var message =
            "Msbuild failed when processing the file 'Acme.Core.csproj' with message: " +
            "The type or namespace name 'Catalog' does not exist in the namespace 'Acme'";
        Assert.False(WorkspaceNoise.IsNuGetAuditNoise(message));
    }

    [Fact]
    public void NullAndUnrelatedMessagesAreKept()
    {
        Assert.False(WorkspaceNoise.IsNuGetAuditNoise(null));
        Assert.False(WorkspaceNoise.IsNuGetAuditNoise(string.Empty));
        Assert.False(WorkspaceNoise.IsNuGetAuditNoise("Invalid project file, unsupported project type"));
    }

    [Theory]
    [InlineData("has a KNOWN vulnerability")]
    [InlineData("HAS A KNOWN VULNERABILITY")]
    public void MatchingIsCaseInsensitive(string message)
    {
        Assert.True(WorkspaceNoise.IsNuGetAuditNoise(message));
    }

    /// The real-world shape this filter exists for: a NuGet package-version
    /// mismatch surfaces as an Msbuild "failure", but the project still loads
    /// and its symbol tree is complete — the index must not carry it as a
    /// warning.
    [Fact]
    public void PackageVersionConflictIsFiltered()
    {
        var message =
            "Msbuild failed when processing the file 'Acme.Publishing.Core.csproj' with message: " +
            "Dependency specified was SonarAnalyzer.CSharp (>= 10.18.0.128626) but ended up with " +
            "SonarAnalyzer.CSharp 10.18.0.131500.";
        Assert.True(WorkspaceNoise.IsPackageVersionConflict(message));
    }

    [Fact]
    public void GenuineLoadFailureIsNotAVersionConflict()
    {
        var message =
            "Msbuild failed when processing the file 'Acme.Core.csproj' with message: " +
            "The type or namespace name 'Catalog' does not exist in the namespace 'Acme'";
        Assert.False(WorkspaceNoise.IsPackageVersionConflict(message));
    }

    [Theory]
    [InlineData(null)]
    [InlineData("")]
    [InlineData("Dependency specified was never met")]           // one half only
    [InlineData("ended up with nothing")]                        // the other half only
    public void IncompleteOrMissingVersionConflictTextIsKept(string? message)
    {
        Assert.False(WorkspaceNoise.IsPackageVersionConflict(message));
    }

    /// The real-world shape this filter exists for: an MSBuild check
    /// advisory (package compatibility) escalates to an Msbuild "failure",
    /// but the project still loads — the index must not carry product
    /// guidance as a warning.
    [Fact]
    public void MsbuildCheckAdvisoryIsFiltered()
    {
        var message =
            "Msbuild failed when processing the file 'Sample.Web.csproj' with message: " +
            "Grpc.Net.ClientFactory 2.63.0 or earlier could cause issues when used together " +
            "with Microsoft.Extensions.Http.Resilience. For more details, see " +
            "https://learn.microsoft.com/dotnet/core/resilience/http-resilience#known-issues. " +
            "Consider using Grpc.Net.ClientFactory 2.64.0 or later. To suppress the warning " +
            "set SuppressCheckGrpcNetClientFactoryVersion=true.";
        Assert.True(WorkspaceNoise.IsMsbuildCheckAdvisory(message));
    }

    [Theory]
    [InlineData(null)]
    [InlineData("")]
    [InlineData("Consider using a newer package")]                          // one half only
    [InlineData("To suppress the warning set Whatever=true")]               // other half only
    [InlineData("could cause issues")]                                      // not the advisory shape
    public void IncompleteOrMissingAdvisoryTextIsKept(string? message)
    {
        Assert.False(WorkspaceNoise.IsMsbuildCheckAdvisory(message));
    }

    /// The real-world shape this filter exists for: after a BuildHost crash
    /// the shutdown diagnostics trail the failure the load exception already
    /// reports — they must not double-count as separate index warnings.
    [Theory]
    [InlineData("[BuildHost PID 58740] Exception while shutting down the BuildHost process.")]
    [InlineData("[BuildHost PID 58740] The BuildHost process is not responding. Process output:")]
    [InlineData("[BuildHost pid 58740] THE BUILDHOST PROCESS IS NOT RESPONDING")]
    public void BuildHostShutdownChatterIsFiltered(string message)
    {
        Assert.True(WorkspaceNoise.IsBuildHostShutdownChatter(message));
    }

    /// The chatter filter must not swallow the actual failure lines: the
    /// BuildHost tag can prefix real errors, and load failures carry it not
    /// at all — both stay warnings.
    [Theory]
    [InlineData(null)]
    [InlineData("")]
    [InlineData("[BuildHost PID 58740] The BuildHost process crashed during project load")]
    [InlineData("Msbuild failed when processing the file 'Acme.Core.csproj' with message: boom")]
    [InlineData("The BuildHost process is not responding.")]   // no BuildHost tag
    public void GenuineBuildHostFailuresAreKept(string? message)
    {
        Assert.False(WorkspaceNoise.IsBuildHostShutdownChatter(message));
    }

    /// The demoted note MUST render the bare message: a failure-kind
    /// diagnostic's ToString() prefixes "[Failure] ", and the Rust capture
    /// substring-matches "[Failure]" anywhere in the line — a demoted note
    /// carrying the tag re-enters the stored index warnings and the demotion
    /// is a no-op.
    [Fact]
    public void DemotedNoteCarriesNoFailureTag()
    {
        var diagnostic = new Microsoft.CodeAnalysis.WorkspaceDiagnostic(
            Microsoft.CodeAnalysis.WorkspaceDiagnosticKind.Failure,
            "Msbuild failed when processing the file 'Acme.Core.csproj' with message: boom");
        Assert.StartsWith("[Failure]", diagnostic.ToString()); // precondition: the tag exists

        var line = WorkspaceNoise.DemotedNote(diagnostic);
        Assert.StartsWith("[INFO] Workspace note: ", line);
        Assert.DoesNotContain("[Failure]", line);
        Assert.Contains("boom", line);
    }
}
