using System.Diagnostics;
using Microsoft.Build.Locator;
using Microsoft.CodeAnalysis.MSBuild;
using ScipCsharp;

namespace ScipCsharp.Tests;

/// <summary>
/// Real-MSBuild workspace for fixture tests. MSBuildLocator registers once
/// per process (a second RegisterInstance throws), so every test class
/// shares this and runs in the "msbuild-workspace" collection.
/// </summary>
internal static class MsbuildTestWorkspace
{
    private const int RestoreTimeoutMs = 180_000;
    private static readonly object RegistrationLock = new();

    internal static string FixturePath(params string[] segments) =>
        Path.Combine([AppContext.BaseDirectory, "Fixtures", .. segments]);

    internal static MSBuildWorkspace Create()
    {
        lock (RegistrationLock)
        {
            if (!MSBuildLocator.IsRegistered)
            {
                var instances = MSBuildLocator.QueryVisualStudioInstances().ToList();
                var sdk = instances.FirstOrDefault(i => i.DiscoveryType == DiscoveryType.DotNetSdk)
                          ?? instances.OrderByDescending(i => i.Version).First();
                MSBuildLocator.RegisterInstance(sdk);
                Environment.SetEnvironmentVariable("MSBUILD_EXE_PATH", sdk.MSBuildPath);
            }
        }

        var workspace = MSBuildWorkspace.Create(WorkspaceLoad.GlobalProperties());
        // Design-time builds emit benign MSBuild warnings; surface, don't fail.
        workspace.WorkspaceFailed += (_, e) =>
            Console.Error.WriteLine($"[INFO] test workspace: {e.Diagnostic.Message}");
        return workspace;
    }

    /// <summary>Restores <paramref name="path"/> (project or solution) unless every project has assets.</summary>
    internal static async Task RestoreIfNeededAsync(string path, params string[] projectDirectories)
    {
        var directories = projectDirectories.Length > 0
            ? projectDirectories
            : [Path.GetDirectoryName(path)!];
        if (directories.All(d => File.Exists(Path.Combine(d, "obj", "project.assets.json"))))
            return;

        var psi = new ProcessStartInfo("dotnet", $"restore \"{path}\"")
        {
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            UseShellExecute = false,
            CreateNoWindow = true,
        };
        using var process = Process.Start(psi)!;
        var stderrTask = process.StandardError.ReadToEndAsync();
        var stdoutTask = process.StandardOutput.ReadToEndAsync();
        if (!process.WaitForExit(RestoreTimeoutMs))
        {
            process.Kill(entireProcessTree: true);
            throw new InvalidOperationException($"dotnet restore timed out for {path}");
        }
        if (process.ExitCode != 0)
        {
            throw new InvalidOperationException(
                $"dotnet restore failed for {path}:\n{await stderrTask}\n{await stdoutTask}");
        }
        _ = await stdoutTask;
    }
}
