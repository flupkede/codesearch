using System.Diagnostics;
using System.Diagnostics.CodeAnalysis;
using Microsoft.Build.Locator;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.MSBuild;

namespace ScipCsharp;

/// <summary>
/// CLI entrypoint for scip-csharp.
///
/// Subcommands:
///   index           — compile solution, collect definitions, write SCIP JSON (fast, no FindReferencesAsync)
///   find-refs       — resolve references for a single symbol on demand (for lazy find_impact caching)
///   batch-find-refs — resolve multiple symbols in one workspace session
///   serve           — resident mode: load the workspace once, answer find-refs/reload
///                     requests as JSON lines on stdin/stdout (todo #115; the Rust host
///                     kills the process for teardown)
/// </summary>
public static class Program
{
    /// <summary>
    /// File extensions that Roslyn/MSBuild cannot load and would crash the BuildHost.
    /// These are skipped when loading a solution.
    /// </summary>
    private static readonly HashSet<string> UnsupportedProjectExtensions = new(StringComparer.OrdinalIgnoreCase)
    {
        ".deployproj",
        ".shproj",
        ".vbproj",
        ".fsproj",
        ".esproj",
        ".sqlproj",
        ".dbproj",
        ".modelproj",
        ".vcxproj",
        ".pyproj",
    };

    public static async Task<int> Main(string[] args)
    {
        if (args.Length == 0 || args[0] is "help" or "--help" or "-h")
        {
            PrintUsage();
            return 0;
        }

        if (args[0] is "--version" or "version")
        {
            // The deployed helper's identity: the host logs it at startup so
            // a stale helper (deployed before its host) is visible.
            Console.WriteLine($"scip-csharp {GetHelperVersion()}");
            return 0;
        }

        return args[0] switch
        {
            "index" => await RunIndexAsync(args[1..]).ConfigureAwait(false),
            "find-refs" => await RunFindRefsAsync(args[1..]).ConfigureAwait(false),
            "batch-find-refs" => await RunBatchFindRefsAsync(args[1..]).ConfigureAwait(false),
            "serve" => await RunServeAsync(args[1..]).ConfigureAwait(false),
            _ => await UnknownCommand(args[0]).ConfigureAwait(false),
        };
    }

    /// <summary>Major.Minor.Build from the assembly — the csproj Version,
    /// kept in sync with the crate by scripts/bump-version.sh.</summary>
    internal static string GetHelperVersion()
    {
        var version = System.Reflection.Assembly.GetEntryAssembly()?.GetName().Version;
        return version is null ? "unknown" : $"{version.Major}.{version.Minor}.{version.Build}";
    }

    /// <summary>
    /// Solution path the serve loop was started with. Remembered so a
    /// "reload" request without an explicit path re-opens the same solution.
    /// Only meaningful in serve mode (resident); other subcommands are
    /// single-shot processes.
    /// </summary>
    internal static string? CurrentServeSolution { get; private set; }

    // ── serve subcommand (resident mode) ─────────────────────────────

    private static async Task<int> RunServeAsync(string[] args)
    {
        var parsed = ParseServeArgs(args);
        if (parsed is null) return 1;

        if (!TryRegisterMsBuild(parsed, out var regErr)) { await Console.Error.WriteLineAsync(regErr).ConfigureAwait(false); return 1; }

        // A resident workspace must not come up mid-restore: the host CACHES
        // its find-refs answers, so degraded answers from a workspace loaded
        // while restore is in flight would be persisted (issue #225
        // acceptance). Startup refuses so the pool falls back to the one-shot
        // path, which handles restore-pending explicitly. Reloads of an
        // ALREADY-RUNNING resident keep answering (gateRestorePending: false).
        if (RestoreGate.ServeStartupPendingExit(parsed) is { } startupPendingExit)
        {
            return startupPendingExit;
        }

        using var workspace = CreateTolerantWorkspace();

        try
        {
            Console.Error.WriteLine($"serve: loading solution: {parsed}");
            await OpenSolutionFilteredAsync(workspace, parsed, gateRestorePending: true).ConfigureAwait(false);
        }
        catch (RestorePendingException)
        {
            // Serve's own startup restore overran its bounded wait: refuse the
            // same way an active marker does — the pool falls back and retries.
            return ExitCodes.RestorePending;
        }
        catch (Exception ex)
        {
            await Console.Error.WriteLineAsync(
                $"serve: [WARN] Solution load partially failed ({ex.GetType().Name}: {ex.Message}); " +
                $"continuing with {workspace.CurrentSolution.Projects.Count()} loaded project(s).")
                .ConfigureAwait(false);

            if (!workspace.CurrentSolution.Projects.Any())
            {
                await Console.Error.WriteLineAsync(
                    $"serve: no projects loaded — cannot serve. Full error:{Environment.NewLine}{ex.StackTrace}")
                    .ConfigureAwait(false);
                return 1;
            }
        }

        CurrentServeSolution = parsed;

        // The loop owns the process lifetime from here: stdin EOF or a
        // "shutdown" request exits 0. The Rust host kills the process for
        // teardown — disposing the Roslyn workspace reliably is not possible,
        // process death is (todo #115).
        return await ServeHost.LoopAsync(
            workspace,
            solution => OpenSolutionFilteredAsync(workspace, solution, gateRestorePending: false)).ConfigureAwait(false);
    }

    private static string? ParseServeArgs(string[] args)
    {
        string? solutionPath = null;
        for (int i = 0; i < args.Length; i++)
        {
            switch (args[i])
            {
                case "--solution":
                    solutionPath = RequireValidPath(args, ref i, "--solution", mustExist: true);
                    if (solutionPath is null) return null;
                    break;
                default:
                    Console.Error.WriteLine($"Unknown argument: {args[i]}");
                    return null;
            }
        }

        if (string.IsNullOrEmpty(solutionPath)) { Console.Error.WriteLine("serve: --solution is required"); return null; }
        return solutionPath;
    }

    // ── index subcommand ─────────────────────────────────────────────

    private static async Task<int> RunIndexAsync(string[] args)
    {
        var totalSw = Stopwatch.StartNew();
        var parsed = ParseIndexArgs(args);
        if (parsed is null) return 1;

        var stageSw = Stopwatch.StartNew();
        if (!TryRegisterMsBuild(parsed.Value.SolutionPath, out var regErr)) { await Console.Error.WriteLineAsync(regErr).ConfigureAwait(false); return 1; }
        StageTiming.Log("msbuild_register", stageSw.Elapsed);

        using var workspace = CreateTolerantWorkspace();

        stageSw.Restart();
        try
        {
            if (!string.IsNullOrEmpty(parsed.Value.SolutionPath))
            {
                Console.Error.WriteLine($"Loading solution: {parsed.Value.SolutionPath}");
                await OpenSolutionFilteredAsync(workspace, parsed.Value.SolutionPath).ConfigureAwait(false);
            }
            else if (!string.IsNullOrEmpty(parsed.Value.ProjectPath))
            {
                // No restore gate on this path: restore (and its marker) is
                // solution-keyed and a bare --project load has no solution.
                // The host never passes --project — out of scope for #225.
                Console.Error.WriteLine($"Loading project: {parsed.Value.ProjectPath}");
                await workspace.OpenProjectAsync(parsed.Value.ProjectPath).ConfigureAwait(false);
            }
            else
            {
                await Console.Error.WriteLineAsync("Either --solution or --project must be specified.").ConfigureAwait(false);
                return 1;
            }
        }
        catch (RestorePendingException)
        {
            // No output file written: restore is still in flight and the
            // Rust host must treat this as restore-pending, not as failure.
            return ExitCodes.RestorePending;
        }
        catch (Exception ex)
        {
            // Some solutions contain unsupported project types (e.g. .deployproj, .shproj) that
            // crash the Roslyn BuildHost.  Log a warning and continue with whatever projects did
            // load — if at least one C# project is available we can still produce a useful index.
            await Console.Error.WriteLineAsync(
                $"[WARN] Solution load partially failed ({ex.GetType().Name}: {ex.Message}); " +
                $"continuing with {workspace.CurrentSolution.Projects.Count()} loaded project(s).")
                .ConfigureAwait(false);

            if (!workspace.CurrentSolution.Projects.Any())
            {
                await Console.Error.WriteLineAsync(
                    "No projects loaded — cannot produce index. Full error:")
                    .ConfigureAwait(false);
                await Console.Error.WriteLineAsync(ex.StackTrace).ConfigureAwait(false);
                return 1;
            }
        }
        finally
        {
            StageTiming.Log("workspace_open", stageSw.Elapsed);
        }

        var indexer = new SymbolIndexer();
        var index = await indexer.IndexAsync(workspace, parsed.Value.ProjectFilter).ConfigureAwait(false);

        stageSw.Restart();
        await OutputWriter.WriteAsync(index, parsed.Value.OutputPath).ConfigureAwait(false);
        StageTiming.Log("write_output", stageSw.Elapsed);

        Console.Error.WriteLine($"Index written to: {parsed.Value.OutputPath}");
        Console.Error.WriteLine($"  Documents: {index.Documents.Count}");
        Console.Error.WriteLine($"  Occurrences: {index.Documents.Sum(d => d.Occurrences.Count)}");
        Console.Error.WriteLine($"  Symbols: {index.ExternalSymbols.Count}");
        Console.Error.WriteLine(index.References is null
            ? "  References: skipped (--filter-project)"
            : $"  References: {index.References.Count}");

        StageTiming.Log("total", totalSw.Elapsed);
        return 0;
    }

    // ── find-refs subcommand ─────────────────────────────────────────

    private static async Task<int> RunFindRefsAsync(string[] args)
    {
        var parsed = ParseFindRefsArgs(args);
        if (parsed is null) return 1;

        if (!TryRegisterMsBuild(parsed.Value.SolutionPath, out var regErr)) { await Console.Error.WriteLineAsync(regErr).ConfigureAwait(false); return 1; }

        using var workspace = CreateTolerantWorkspace();

        try
        {
            Console.Error.WriteLine($"find-refs: loading solution: {parsed.Value.SolutionPath}");
            await OpenSolutionFilteredAsync(workspace, parsed.Value.SolutionPath).ConfigureAwait(false);
        }
        catch (RestorePendingException)
        {
            // No output file written: the Rust host maps exit 75 to a
            // restore-pending outcome, never a generic resolver failure.
            return ExitCodes.RestorePending;
        }
        catch (Exception ex)
        {
            await Console.Error.WriteLineAsync(
                $"find-refs: [WARN] Solution load partially failed ({ex.GetType().Name}: {ex.Message}); " +
                $"continuing with {workspace.CurrentSolution.Projects.Count()} loaded project(s).")
                .ConfigureAwait(false);

            if (!workspace.CurrentSolution.Projects.Any())
            {
                await Console.Error.WriteLineAsync(
                    $"find-refs: no projects loaded — cannot resolve refs. Full error:{Environment.NewLine}{ex.StackTrace}")
                    .ConfigureAwait(false);
                return 1;
            }
        }

        var resolver = new ReferenceResolver();
        FindRefsOutput result;
        try
        {
            result = await resolver.FindRefsAsync(
                workspace.CurrentSolution,
                parsed.Value.Symbol).ConfigureAwait(false);
        }
        catch (Exception ex)
        {
            await Console.Error.WriteLineAsync(
                $"find-refs: resolver failed: {ex.GetType().Name}: {ex.Message}{Environment.NewLine}{ex.StackTrace}")
                .ConfigureAwait(false);
            return 1;
        }

        await OutputWriter.WriteRefsAsync(result, parsed.Value.OutputPath).ConfigureAwait(false);

        Console.Error.WriteLine($"find-refs: output written to {parsed.Value.OutputPath}");
        Console.Error.WriteLine($"find-refs: {result.References.Count} reference(s)");
        return 0;
    }

    // ── batch-find-refs subcommand ──────────────────────────────────

    private static async Task<int> RunBatchFindRefsAsync(string[] args)
    {
        var parsed = ParseBatchFindRefsArgs(args);
        if (parsed is null) return 1;

        if (!TryRegisterMsBuild(parsed.Value.SolutionPath, out var regErr)) { await Console.Error.WriteLineAsync(regErr).ConfigureAwait(false); return 1; }

        using var workspace = CreateTolerantWorkspace();

        try
        {
            Console.Error.WriteLine($"batch-find-refs: loading solution: {parsed.Value.SolutionPath}");
            await OpenSolutionFilteredAsync(workspace, parsed.Value.SolutionPath).ConfigureAwait(false);
        }
        catch (RestorePendingException)
        {
            // No output file written: the Rust host maps exit 75 to a
            // restore-pending outcome, never a generic resolver failure.
            return ExitCodes.RestorePending;
        }
        catch (Exception ex)
        {
            await Console.Error.WriteLineAsync(
                $"batch-find-refs: [WARN] Solution load partially failed ({ex.GetType().Name}: {ex.Message}); " +
                $"continuing with {workspace.CurrentSolution.Projects.Count()} loaded project(s).")
                .ConfigureAwait(false);

            if (!workspace.CurrentSolution.Projects.Any())
            {
                await Console.Error.WriteLineAsync(
                    $"batch-find-refs: no projects loaded — cannot resolve refs. Full error:{Environment.NewLine}{ex.StackTrace}")
                    .ConfigureAwait(false);
                return 1;
            }
        }

        Console.Error.WriteLine($"batch-find-refs: loaded {parsed.Value.Symbols.Count} symbol(s) from input");

        var resolver = new ReferenceResolver();
        BatchFindRefsOutput result;
        try
        {
            result = await resolver.BatchFindRefsAsync(
                workspace.CurrentSolution,
                parsed.Value.Symbols).ConfigureAwait(false);
        }
        catch (Exception ex)
        {
            await Console.Error.WriteLineAsync(
                $"batch-find-refs: resolver failed: {ex.GetType().Name}: {ex.Message}{Environment.NewLine}{ex.StackTrace}")
                .ConfigureAwait(false);
            return 1;
        }

        await OutputWriter.WriteBatchRefsAsync(result, parsed.Value.OutputPath).ConfigureAwait(false);

        var totalRefs = result.Results.Sum(r => r.References.Count);
        Console.Error.WriteLine($"batch-find-refs: output written to {parsed.Value.OutputPath}");
        Console.Error.WriteLine($"batch-find-refs: {result.Results.Count} symbols, {totalRefs} total reference(s)");
        return 0;
    }

    // ── Workspace creation ───────────────────────────────────────────

    /// <summary>
    /// Creates an <see cref="MSBuildWorkspace"/> that logs workspace diagnostics as warnings
    /// instead of throwing, so unsupported project types (.deployproj, .shproj, etc.) are
    /// skipped gracefully instead of crashing the entire solution load.
    /// </summary>
    private static MSBuildWorkspace CreateTolerantWorkspace()
    {
        var properties = WorkspaceLoad.GlobalProperties();

        // If MSBUILD_EXE_PATH is set, pass it to the workspace so the BuildHost
        // uses the same MSBuild we registered via MSBuildLocator (typically the .NET SDK).
        // This prevents the BuildHost from falling back to a VS-installed MSBuild
        // that may be incompatible (e.g. .NET Framework 4.8 vs .NET 10).
        var msbuildExePath = Environment.GetEnvironmentVariable("MSBUILD_EXE_PATH");
        if (!string.IsNullOrEmpty(msbuildExePath) && File.Exists(msbuildExePath))
        {
            properties["MSBUILD_EXE_PATH"] = msbuildExePath;
        }

        var workspace = MSBuildWorkspace.Create(properties);
        workspace.WorkspaceFailed += (_, e) =>
        {
            // Warning-severity diagnostics are benign resolution notes (e.g.
            // "Found project reference without a matching metadata reference"
            // for excluded or external projects): they do not block indexing
            // and would pin the repo's index warning forever. Only true
            // failures belong in the warning channel.
            if (e.Diagnostic.Kind == WorkspaceDiagnosticKind.Warning)
            {
                Console.Error.WriteLine($"[INFO] Workspace note: {e.Diagnostic}");
                return;
            }
            // NuGet's vulnerability audit surfaces as an Msbuild "failure" but
            // is a security advisory, not a load failure — the index is
            // unaffected and emitting it would pin the repo's index warning
            // until the package is bumped.
            if (WorkspaceNoise.IsNuGetAuditNoise(e.Diagnostic?.Message))
            {
                return;
            }
            // A NuGet package-version mismatch ("Dependency specified was X
            // (>= A) but ended up with X B") also surfaces as an Msbuild
            // "failure", but the affected project still loads and its symbol
            // tree is complete — build engineering settles the pin; the
            // index must not carry the noise as a warning.
            if (WorkspaceNoise.IsPackageVersionConflict(e.Diagnostic?.Message)
                || WorkspaceNoise.IsNuGetSourceFailure(e.Diagnostic?.Message))
            {
                Console.Error.WriteLine(WorkspaceNoise.DemotedNote(e.Diagnostic!));
                return;
            }
            // MSBuild check advisories (package compatibility, e.g. the
            // Grpc.Net.ClientFactory version check) also escalate to
            // "failures": product guidance, not load failures — demote.
            if (WorkspaceNoise.IsMsbuildCheckAdvisory(e.Diagnostic?.Message))
            {
                Console.Error.WriteLine(WorkspaceNoise.DemotedNote(e.Diagnostic!));
                return;
            }
            // BuildHost shutdown chatter ("[BuildHost PID N] ... not
            // responding") trails a crash the load failure already reports;
            // keeping it would double-count one failure.
            if (WorkspaceNoise.IsBuildHostShutdownChatter(e.Diagnostic?.Message))
            {
                Console.Error.WriteLine(WorkspaceNoise.DemotedNote(e.Diagnostic!));
                return;
            }
            Console.Error.WriteLine($"[WARN] Workspace error: {e.Diagnostic}");
        };
        return workspace;
    }

    /// A project whose own restore failed during the per-project fallback.
    /// With usable existing assets it is still indexed — its compilation
    /// diagnostics are restore-caused (reported as [INFO], never driving the
    /// warning icon); without assets it is excluded from the load entirely.
    private sealed record ProjectRestoreFailure(string ProjectName, string RelativePath, string FirstError);

    /// <summary>
    /// Restores the solution before the design-time load. MSBuildWorkspace
    /// does not restore: with stale or missing obj/project.assets.json,
    /// package-typed references fail to resolve and the load drowns in CS0246
    /// cascades even though `dotnet build` (which restores first) succeeds.
    /// Gated by <see cref="RestorePolicy.NeedsRestore"/>.
    ///
    /// Restore is the ONLY all-or-nothing step in NuGet: one unresolvable
    /// package anywhere fails the solution restore for EVERY project. That
    /// is why a failing solution restore falls back to per-project restores:
    /// each project that succeeds gets fresh assets and is indexed fully;
    /// each project that fails keeps its existing (usable) assets and is
    /// reported as a restore-caused caveat instead of drowning the index in
    /// unrelated CS noise. Best effort throughout: a failed or missing
    /// `dotnet` is logged and the load proceeds. The wait is bounded, but a
    /// running restore is never killed: on timeout it continues in the
    /// background and the next reindex picks up the fresh assets.
    /// </summary>
    /// <returns>The per-project failures from the fallback (empty when the
    /// solution-level restore succeeded or nothing needed restoring).</returns>
    private static List<ProjectRestoreFailure> RestoreSolution(
        string solutionPath,
        bool gateRestorePending,
        List<(int LineIndex, string RelativePath)> loadable)
    {
        var timeoutSeconds = RestorePolicy.RestoreTimeoutSeconds();
        // Never start a second restore while one is already running for this
        // solution: an ACTIVE marker means another process (or an earlier
        // timed-out run of this one) still has dotnet restore going. A stale
        // marker is deleted by the check and the run proceeds normally.
        if (RestorePendingMarker.IsActive(solutionPath))
        {
            if (gateRestorePending)
            {
                Console.Error.WriteLine(
                    "[RESTORE-PENDING] a dotnet restore for this solution is already running — " +
                    $"exiting {ExitCodes.RestorePending} without building an index; retry when it finishes.");
                throw new RestorePendingException();
            }
            Console.Error.WriteLine(
                "[INFO] a dotnet restore for this solution is already running — serve reload " +
                "keeps answering from the existing workspace until it finishes.");
            return [];
        }
        try
        {
            var psi = new ProcessStartInfo
            {
                FileName = "dotnet",
                // --ignore-failed-sources: an unreachable private feed must not
                // block indexing when the packages are already in the cache.
                Arguments = $"restore \"{solutionPath}\" --nologo -v q --ignore-failed-sources",
                RedirectStandardError = true,
                RedirectStandardOutput = true,
                UseShellExecute = false,
            };
            using var process = Process.Start(psi);
            if (process is null)
            {
                return [];
            }
            // Drain both pipes in the background: the child inherits these
            // handles, so without readers a chatty restore could block on a
            // full pipe, and the serve process reads stderr until EOF. The
            // captured stderr feeds the failure diagnostic below — a failed
            // restore without its NU11xx reason (unreachable private feed)
            // just reads as unexplained CS0518/CS0246 noise downstream.
            var stdoutDrain = process.StandardOutput.ReadToEndAsync();
            var stderrDrain = process.StandardError.ReadToEndAsync();
            // Bounded WAIT, never a kill: a killed restore wastes all of its
            // work and leaves assets missing, so every reindex would pay the
            // same wait and stay degraded. On timeout the restore keeps
            // running in the background (the child outlives this process) and
            // is recorded in a marker so later runs can detect it. Gated
            // commands refuse to build an index (exit RestorePending) — a
            // degraded index stored now would poison LMDB until the next
            // rebuild — while serve keeps answering from the existing
            // workspace. Timeout is env-tunable for large solutions.
            if (!process.WaitForExit(timeoutSeconds * 1000))
            {
                RestorePendingMarker.Write(
                    solutionPath, process.Id, process.StartTime.ToUniversalTime());
                if (gateRestorePending)
                {
                    Console.Error.WriteLine(
                        $"[RESTORE-PENDING] dotnet restore still running after {timeoutSeconds}s for " +
                        $"'{solutionPath}' — not building an index while restore is in flight; the " +
                        "previous index stays live. Retry when restore finishes, or raise " +
                        $"{RestorePolicy.TimeoutEnvName}.");
                    throw new RestorePendingException();
                }
                Console.Error.WriteLine(RestorePolicy.StillRunningWarning(timeoutSeconds));
                return [];
            }
            if (process.ExitCode == 0)
            {
                return [];
            }
            // NETSDK1045 and friends print to STDOUT on current dotnet hosts
            // (NU11xx detail may land on either stream) — classify over both.
            var restoreOutput = stderrDrain.GetAwaiter().GetResult()
                + Environment.NewLine + stdoutDrain.GetAwaiter().GetResult();
            var nuDetail = RestorePolicy.ExtractRestoreErrors(restoreOutput);
            if (MsbuildRestore.IsSdkResolutionFailure(restoreOutput))
            {
                // The CLI SDK set cannot even target this solution — packages
                // were never the problem, so the per-project fallback (same
                // dotnet, same SDK wall) can only re-fail per project. A
                // Visual Studio install ships its own MSBuild + SDK targets
                // and can restore the full solution in one go.
                var vsMsbuild = MsbuildRestore.LocateVsMsbuild();
                if (vsMsbuild is null)
                {
                    Console.Error.WriteLine(
                        "[INFO] no VS MSBuild found via vswhere — the CLI SDK gap cannot be worked around.");
                }
                else
                {
                    Console.Error.WriteLine(
                        "[INFO] the CLI .NET SDK set cannot target this solution — retrying the restore " +
                        $"with VS MSBuild at '{vsMsbuild}'.");
                    switch (TryRestoreSolutionWithVsMsbuild(
                        vsMsbuild, solutionPath, timeoutSeconds, gateRestorePending))
                    {
                        case VsRestoreOutcome.Restored:
                            Console.Error.WriteLine(
                                "[INFO] VS MSBuild restore succeeded — full solution assets restored.");
                            return [];
                        case VsRestoreOutcome.StillRunningUngated:
                            // The VS restore keeps writing obj/ in the
                            // background; per-project dotnet restores would
                            // race those writes, so degrade with an honest
                            // per-project reason instead.
                            return loadable.Select(e => new ProjectRestoreFailure(
                                Path.GetFileNameWithoutExtension(e.RelativePath),
                                e.RelativePath,
                                "not attempted — a VS MSBuild restore is still running")).ToList();
                    }
                    // Failed → the per-project fallback below still runs: in a
                    // mixed-TFM solution it recovers the projects the CLI CAN
                    // target.
                }
            }
            var fallback = RestorePerProjectFallback(
                solutionPath, solutionDir: Path.GetDirectoryName(solutionPath) ?? ".",
                loadable, timeoutSeconds, gateRestorePending);
            var restoredCount = loadable.Count - fallback.Count;
            var header =
                $"dotnet restore failed (exit {process.ExitCode}) for '{solutionPath}' — " +
                $"per-project restore recovered {restoredCount}/{loadable.Count} project(s)." +
                nuDetail;
            if (fallback.Count == 0)
            {
                Console.Error.WriteLine($"[INFO] {header}");
            }
            else
            {
                // INFO, never WARN — even when nothing recovered: a restore
                // failure (a dead private feed, for one) is environment
                // state, not a symbol-index defect, and must not drive the
                // caller-facing warning icon. The cause rides in serve.log;
                // per-project failures are reported below either way.
                Console.Error.WriteLine(
                    "[INFO] " + header +
                    " Every project failed its own restore; continuing with a degraded index.");
                foreach (var failure in fallback)
                {
                    Console.Error.WriteLine(
                        $"[INFO] {failure.ProjectName}: restore failed — indexed from existing " +
                        $"assets; its compile diagnostics are restore-caused. {failure.FirstError}");
                }
            }
            return fallback;
        }
        catch (RestorePendingException)
        {
            throw;
        }
        catch (Exception ex)
        {
            Console.Error.WriteLine(
                $"[INFO] dotnet restore could not start ({ex.GetType().Name}: {ex.Message}) — continuing without it.");
            return [];
        }
    }

    /// <summary>Outcome of the one-shot VS-MSBuild solution restore.</summary>
    private enum VsRestoreOutcome
    {
        Restored,
        Failed,
        /// <summary>Timed out ungated: the restore keeps running, serve degrades.</summary>
        StillRunningUngated,
    }

    /// <summary>
    /// One bounded attempt to restore the solution with a VS MSBuild instead
    /// of the CLI dotnet — the fallback for SDK-resolution failures, where
    /// the machine's Visual Studio can do what its CLI SDK set cannot. Same
    /// contract as the dotnet restore above: never killed on timeout (the
    /// marker gates later runs), gated commands exit
    /// <see cref="ExitCodes.RestorePending"/> instead of building a degraded
    /// index. <c>-nr:false</c> because the helper is a short-lived subprocess
    /// on a hot serve loop and must not leave MSBuild worker nodes behind.
    /// </summary>
    private static VsRestoreOutcome TryRestoreSolutionWithVsMsbuild(
        string msbuildPath,
        string solutionPath,
        int timeoutSeconds,
        bool gateRestorePending)
    {
        try
        {
            var psi = new ProcessStartInfo
            {
                FileName = msbuildPath,
                Arguments = $"\"{solutionPath}\" -t:restore -nologo -v:q -nr:false",
                RedirectStandardError = true,
                RedirectStandardOutput = true,
                UseShellExecute = false,
            };
            using var process = Process.Start(psi);
            if (process is null)
            {
                return VsRestoreOutcome.Failed;
            }
            _ = process.StandardOutput.ReadToEndAsync();
            var stderrDrain = process.StandardError.ReadToEndAsync();
            if (!process.WaitForExit(timeoutSeconds * 1000))
            {
                RestorePendingMarker.Write(
                    solutionPath, process.Id, process.StartTime.ToUniversalTime());
                if (gateRestorePending)
                {
                    Console.Error.WriteLine(
                        "[RESTORE-PENDING] the VS MSBuild restore is still running — exiting " +
                        $"{ExitCodes.RestorePending} without building an index; retry when it finishes.");
                    throw new RestorePendingException();
                }
                Console.Error.WriteLine(RestorePolicy.StillRunningWarning(timeoutSeconds, "VS MSBuild restore"));
                return VsRestoreOutcome.StillRunningUngated;
            }
            if (process.ExitCode == 0)
            {
                return VsRestoreOutcome.Restored;
            }
            Console.Error.WriteLine(
                "[INFO] VS MSBuild restore failed" +
                RestorePolicy.ExtractRestoreErrors(stderrDrain.GetAwaiter().GetResult()));
            return VsRestoreOutcome.Failed;
        }
        catch (RestorePendingException)
        {
            throw;
        }
        catch (Exception ex)
        {
            Console.Error.WriteLine(
                $"[INFO] VS MSBuild restore could not start ({ex.GetType().Name}: {ex.Message}).");
            return VsRestoreOutcome.Failed;
        }
    }

    /// <summary>
    /// The per-project fallback after a failed solution restore: NuGet is
    /// all-or-nothing per restore, so one dead package fails every project —
    /// but most projects restore fine on their own. Same bounded-wait rules
    /// as the solution restore: a project restore that outlives the remaining
    /// deadline is recorded as pending (gated commands refuse) and never
    /// killed; the overall deadline keeps the fallback from multiplying the
    /// solution timeout by the project count.
    /// </summary>
    private static List<ProjectRestoreFailure> RestorePerProjectFallback(
        string solutionPath,
        string solutionDir,
        List<(int LineIndex, string RelativePath)> loadable,
        int timeoutSeconds,
        bool gateRestorePending)
    {
        var failures = new List<ProjectRestoreFailure>();
        var deadline = DateTime.UtcNow.AddSeconds(timeoutSeconds);
        for (var i = 0; i < loadable.Count; i++)
        {
            var entry = loadable[i];
            var remainingMs = (int)(deadline - DateTime.UtcNow).TotalMilliseconds;
            if (remainingMs <= 0)
            {
                // Budget spent: never-attempted projects must be classified
                // as failures too — their stale-asset diagnostics are exactly
                // as restore-caused as an explicit NU1101, and counting them
                // as "recovered" would re-apply the warning icon to noise.
                // They are retried on the next reindex.
                Console.Error.WriteLine(
                    "[INFO] per-project restore budget exhausted; remaining project(s) keep their " +
                    "existing assets and are retried on the next reindex.");
                failures.AddRange(loadable.Skip(i).Select(e => new ProjectRestoreFailure(
                    Path.GetFileNameWithoutExtension(e.RelativePath),
                    e.RelativePath,
                    "not attempted — per-project restore budget exhausted")));
                break;
            }
            var projectPath = Path.GetFullPath(Path.Combine(solutionDir, entry.RelativePath));
            try
            {
                var psi = new ProcessStartInfo
                {
                    FileName = "dotnet",
                    Arguments = $"restore \"{projectPath}\" --nologo -v q --ignore-failed-sources",
                    RedirectStandardError = true,
                    RedirectStandardOutput = true,
                    UseShellExecute = false,
                };
                using var process = Process.Start(psi);
                if (process is null)
                {
                    failures.Add(new ProjectRestoreFailure(
                        Path.GetFileNameWithoutExtension(entry.RelativePath),
                        entry.RelativePath,
                        "dotnet could not be started"));
                    continue;
                }
                _ = process.StandardOutput.ReadToEndAsync();
                var stderrDrain = process.StandardError.ReadToEndAsync();
                if (!process.WaitForExit(remainingMs))
                {
                    RestorePendingMarker.Write(
                        solutionPath, process.Id, process.StartTime.ToUniversalTime());
                    if (gateRestorePending)
                    {
                        Console.Error.WriteLine(
                            $"[RESTORE-PENDING] per-project restore of '{entry.RelativePath}' is still " +
                            $"running — exiting {ExitCodes.RestorePending} without building an index; " +
                            "retry when it finishes.");
                        throw new RestorePendingException();
                    }
                    Console.Error.WriteLine(RestorePolicy.StillRunningWarning(timeoutSeconds));
                    failures.AddRange(loadable.Skip(i).Select(e => new ProjectRestoreFailure(
                        Path.GetFileNameWithoutExtension(e.RelativePath),
                        e.RelativePath,
                        "not attempted — a per-project restore is still running (budget exhausted)")));
                    break;
                }
                if (process.ExitCode != 0)
                {
                    failures.Add(new ProjectRestoreFailure(
                        Path.GetFileNameWithoutExtension(entry.RelativePath),
                        entry.RelativePath,
                        RestorePolicy.ExtractRestoreErrors(stderrDrain.GetAwaiter().GetResult())
                            .Replace(" Restore output: ", "").Trim()));
                }
            }
            catch (RestorePendingException)
            {
                throw;
            }
            catch (Exception ex)
            {
                Console.Error.WriteLine(
                    $"[INFO] per-project restore could not start for '{entry.RelativePath}' " +
                    $"({ex.GetType().Name}: {ex.Message}).");
                failures.Add(new ProjectRestoreFailure(
                    Path.GetFileNameWithoutExtension(entry.RelativePath),
                    entry.RelativePath,
                    $"could not start ({ex.GetType().Name})"));
            }
        }
        return failures;
    }

    // ── Solution filtering ───────────────────────────────────────────

    /// <summary>
    /// Opens a solution but first checks if it contains unsupported project types.
    /// If unsupported projects are found, creates a filtered temporary .sln that
    /// excludes them, then opens that instead.
    /// If the filtered solution still fails to load (e.g. BuildHost crash), falls
    /// back to loading individual .csproj files from the solution.
    /// </summary>
    private static async Task OpenSolutionFilteredAsync(
        MSBuildWorkspace workspace,
        string solutionPath,
        bool gateRestorePending = true)
    {
        var solutionDir = Path.GetDirectoryName(solutionPath) ?? ".";
        var lines = await File.ReadAllLinesAsync(solutionPath).ConfigureAwait(false);

        // Parse project entries from .sln: Project("{GUID}") = "Name", "RelativePath", "{GUID}"
        // Paths that resolve outside solutionDir are rejected (path traversal protection).
        var projectEntries = ParseSolutionProjects(lines, solutionDir);

        // Separate supported vs unsupported projects
        var supported = projectEntries
            .Where(p => !UnsupportedProjectExtensions.Contains(Path.GetExtension(p.RelativePath)))
            .ToList();
        var unsupported = projectEntries
            .Where(p => UnsupportedProjectExtensions.Contains(Path.GetExtension(p.RelativePath)))
            .ToList();

        // End-of-life frameworks (netcoreapp 1–3): a modern SDK builds them,
        // but the design-time load cannot resolve their reference assemblies
        // and every type use cascades into CS0518 — tool-side noise, not code
        // health. Skipped with an [INFO] note; never restored (we will not
        // compile them).
        var loadable = new List<(int LineIndex, string RelativePath)>();
        var eolSkipped = new List<(int LineIndex, string RelativePath, string Tfm)>();
        foreach (var entry in supported)
        {
            var projectPath = Path.GetFullPath(Path.Combine(solutionDir, entry.RelativePath));
            string? tfm = null;
            try
            {
                if (File.Exists(projectPath))
                {
                    tfm = SkippedFrameworks.ExtractFromProjectText(File.ReadAllText(projectPath));
                }
            }
            catch (Exception)
            {
                // Unreadable csproj: let the workspace load surface the real
                // problem instead of guessing here.
            }
            if (tfm is not null && SkippedFrameworks.IsEndOfLife(tfm))
            {
                eolSkipped.Add((entry.LineIndex, entry.RelativePath, tfm));
            }
            else
            {
                loadable.Add(entry);
            }
        }
        if (eolSkipped.Count > 0)
        {
            Console.Error.WriteLine(
                $"[INFO] Skipping {eolSkipped.Count} project(s) with end-of-life frameworks " +
                $"(not indexed): {string.Join(", ", eolSkipped.Select(e => $"{e.RelativePath} ({e.Tfm})"))}");
        }

        // Restore only when the evidence says assets are missing or stale: the
        // design-time build below does not restore, and missing package assets
        // cascade into CS0246 noise that would otherwise pin the repo's index
        // warning. But restoring on every reindex is far too heavy for
        // multi-repo hubs — fresh assets mean no restore at all.
        //
        // Gate: while a restore is in flight, gated commands (index,
        // find-refs, batch-find-refs) refuse to load the workspace at all, so
        // no index or cached find-refs answer can ever be produced from a
        // solution loaded mid-restore. Serve passes gateRestorePending: false
        // — a resident process must keep answering from the existing index.
        if (gateRestorePending)
        {
            RestoreGate.ThrowIfRestorePending(solutionPath);
        }
        var restoreFailures = new List<ProjectRestoreFailure>();
        if (RestorePolicy.NeedsRestore(loadable.Select(p => p.RelativePath), solutionDir))
        {
            restoreFailures = RestoreSolution(solutionPath, gateRestorePending, loadable);
        }

        // Restore-failed projects fall in two classes: with usable existing
        // assets they are indexed as usual — their compile diagnostics are
        // code-health ([INFO], never the warning icon); without any assets
        // there is nothing to compile against and they are excluded outright.
        var noAssets = restoreFailures
            .Where(f => !RestorePolicy.HasUsableAssets(f.RelativePath, solutionDir))
            .ToList();
        if (noAssets.Count > 0)
        {
            Console.Error.WriteLine(
                $"[INFO] Excluding {noAssets.Count} project(s) with no usable package assets " +
                $"(not indexed): {string.Join(", ", noAssets.Select(f => f.RelativePath))}");
        }

        // Log skipped projects
        foreach (var skip in unsupported)
        {
            Console.Error.WriteLine($"[INFO] Skipping unsupported project type: {skip.RelativePath}");
        }

        if (loadable.Count == noAssets.Count)
        {
            throw new InvalidOperationException(
                $"No project of the solution can be indexed " +
                $"({noAssets.Count} failed restore with no existing assets, " +
                $"{eolSkipped.Count} end-of-life frameworks, {unsupported.Count} unsupported types)");
        }

        // Try opening the filtered solution first: unsupported types,
        // end-of-life frameworks and restore-failed projects without assets
        // are all excluded — the workspace must never see them.
        var excludedPaths = noAssets.Select(f => f.RelativePath).ToHashSet(StringComparer.OrdinalIgnoreCase);
        var skipIndices = unsupported.Select(u => u.LineIndex)
            .Concat(eolSkipped.Select(e => e.LineIndex))
            .Concat(loadable.Where(l => excludedPaths.Contains(l.RelativePath)).Select(l => l.LineIndex))
            .ToHashSet();
        var filteredContent = BuildFilteredSolution(lines, skipIndices);
        var tempSlnName = $"{Path.GetFileNameWithoutExtension(solutionPath)}-filtered-{Guid.NewGuid():N}.sln";
        var tempSlnPath = Path.Combine(solutionDir, tempSlnName);
        await File.WriteAllTextAsync(tempSlnPath, filteredContent).ConfigureAwait(false);

        try
        {
            await workspace.OpenSolutionAsync(tempSlnPath).ConfigureAwait(false);
            Console.Error.WriteLine($"Loaded {workspace.CurrentSolution.Projects.Count()} project(s) from filtered solution");
            return; // Success!
        }
        catch (Exception ex)
        {
            var level = WorkspaceNoise.IsBuildHostTypeInitializerCrash(ex.Message) ? "INFO" : "WARN";
            Console.Error.WriteLine(
                $"[{level}] Filtered solution still failed ({ex.GetType().Name}: {ex.Message}); " +
                $"falling back to loading {loadable.Count - noAssets.Count} individual .csproj files.");
        }
        finally
        {
            try { File.Delete(tempSlnPath); } catch { /* best effort */ }
        }

        // Fallback: open each loadable .csproj individually (never the EOL or
        // no-assets exclusions — the workspace must not see them here either).
        // CloseSolution() clears any partially-loaded projects from the failed solution attempt.
        workspace.CloseSolution();
        var loaded = 0;
        foreach (var entry in loadable)
        {
            var csprojPath = Path.GetFullPath(Path.Combine(solutionDir, entry.RelativePath));

            if (excludedPaths.Contains(entry.RelativePath))
            {
                continue;
            }

            // Defense-in-depth: verify containment even though parsing already validates.
            if (!IsPathWithinRoot(csprojPath, solutionDir))
            {
                Console.Error.WriteLine(
                    $"[WARN] Skipping project outside solution root: '{csprojPath}'");
                continue;
            }

            if (!File.Exists(csprojPath))
            {
                Console.Error.WriteLine($"[WARN] Project file not found: {csprojPath}");
                continue;
            }

            // OpenProjectAsync also loads the project's ProjectReferences; re-opening
            // one of them throws "already part of the workspace".
            if (workspace.CurrentSolution.Projects.Any(p =>
                    string.Equals(p.FilePath, csprojPath, StringComparison.OrdinalIgnoreCase)))
            {
                loaded++;
                continue;
            }

            try
            {
                await workspace.OpenProjectAsync(csprojPath).ConfigureAwait(false);
                loaded++;
            }
            catch (Exception ex)
            {
                var level = WorkspaceNoise.IsBuildHostTypeInitializerCrash(ex.Message) ? "INFO" : "WARN";
                Console.Error.WriteLine(
                    $"[{level}] Failed to load project '{entry.RelativePath}': {ex.GetType().Name}: {ex.Message}");
            }
        }

        Console.Error.WriteLine($"Loaded {loaded}/{supported.Count} project(s) individually");
    }

    /// <summary>
    /// Parses project entries from .sln file lines.
    /// Skips solution folders (GUID {2150E333-...}) which have no file path.
    /// Rejects project paths that resolve outside the solution directory (path traversal).
    /// </summary>
    private static List<(int LineIndex, string RelativePath)> ParseSolutionProjects(
        string[] lines, string solutionDir)
    {
        var result = new List<(int, string)>();
        for (int i = 0; i < lines.Length; i++)
        {
            var line = lines[i].Trim();
            if (!line.StartsWith("Project(", StringComparison.Ordinal)) continue;

            var parts = SplitProjectLine(line);
            if (parts is null || parts.Length < 2) continue;

            var relativePath = parts[1];
            // Solution folders have the folder name as both "Name" and "Path" (no extension).
            // Skip entries that don't look like file paths.
            if (!Path.HasExtension(relativePath)) continue;

            // Security: reject paths that escape the solution directory.
            var fullProjectPath = Path.GetFullPath(Path.Combine(solutionDir, relativePath));
            if (!IsPathWithinRoot(fullProjectPath, solutionDir))
            {
                Console.Error.WriteLine(
                    $"[WARN] Skipping project with path traversal: '{relativePath}' " +
                    $"resolves to '{fullProjectPath}' which is outside solution root '{solutionDir}'");
                continue;
            }

            result.Add((i, relativePath));
        }
        return result;
    }

    /// <summary>
    /// Checks that <paramref name="path"/> is contained within <paramref name="root"/>.
    /// Both paths are normalized via <see cref="Path.GetFullPath"/> before comparison.
    /// Returns true if <paramref name="path"/> equals root or is a descendant.
    /// </summary>
    private static bool IsPathWithinRoot(string path, string root)
    {
        // Normalize both to handle mixed separators, redundant dots, etc.
        var normalizedPath = Path.GetFullPath(path);
        var normalizedRoot = Path.GetFullPath(root);

        // Ensure root ends with separator so "/foo" doesn't match "/foobar"
        var rootPrefix = normalizedRoot;
        if (!rootPrefix.EndsWith(Path.DirectorySeparatorChar) &&
            !rootPrefix.EndsWith(Path.AltDirectorySeparatorChar))
        {
            rootPrefix += Path.DirectorySeparatorChar;
        }

        return normalizedPath.StartsWith(rootPrefix, StringComparison.OrdinalIgnoreCase) ||
               string.Equals(normalizedPath, normalizedRoot, StringComparison.OrdinalIgnoreCase);
    }

    /// <summary>
    /// Builds a new .sln content string with the specified project line indices removed.
    /// Removes both the Project() line and its corresponding EndProject line.
    /// </summary>
    private static string BuildFilteredSolution(string[] lines, HashSet<int> skipLineIndices)
    {
        var result = new List<string>(lines.Length);
        var skipping = false;

        for (int i = 0; i < lines.Length; i++)
        {
            if (skipLineIndices.Contains(i))
            {
                skipping = true;
                continue; // skip the Project() line
            }

            if (skipping)
            {
                // Look for the EndProject that closes this project block
                if (lines[i].Trim() == "EndProject")
                {
                    skipping = false;
                    continue; // skip the EndProject line too
                }
                // Shouldn't happen — project lines are always followed by EndProject,
                // but skip any intermediate lines just in case.
                continue;
            }

            result.Add(lines[i]);
        }

        return string.Join(Environment.NewLine, result);
    }

    /// <summary>
    /// Splits a .sln Project() line into its quoted segments.
    /// Returns null if the line doesn't match the expected format.
    /// </summary>
    private static string[]? SplitProjectLine(string line)
    {
        var eqIdx = line.IndexOf('=');
        if (eqIdx < 0) return null;

        var afterEq = line.AsSpan()[(eqIdx + 1)..];
        var segments = new List<string>();
        int start = -1;

        for (int i = 0; i < afterEq.Length; i++)
        {
            if (afterEq[i] == '"')
            {
                if (start < 0)
                    start = i + 1;
                else
                {
                    segments.Add(afterEq.Slice(start, i - start).ToString());
                    start = -1;
                }
            }
        }

        return segments.Count >= 2 ? segments.ToArray() : null;
    }

    // ── MSBuild registration ─────────────────────────────────────────

    /// <summary>
    /// Register the MSBuild instance the workspace will evaluate through.
    /// Capability-aware since the 2026-10-15 net10 incident: when every
    /// CLI-visible .NET SDK is older than the repo's target frameworks
    /// (raw <c>TargetFramework</c> scan + global.json floor via
    /// <see cref="MsbuildRestore.RequiredSdkMajorFloor"/>), an SDK instance
    /// would fail every project with NETSDK1045 during evaluation — in that
    /// case the highest VS instance wins instead, because VS bundles its own
    /// SDK targets and can evaluate TFMs the CLI cannot. Returns true on
    /// success; sets <paramref name="errorMessage"/> on failure.
    /// </summary>
    private static bool TryRegisterMsBuild(
        string? solutionPath,
        [System.Diagnostics.CodeAnalysis.NotNullWhen(false)] out string? errorMessage)
    {
        // Guard against double-registration in case the function is ever called twice
        // in the same process lifetime (e.g. future test harnesses, daemon mode).
        if (MSBuildLocator.IsRegistered)
        {
            errorMessage = null;
            return true;
        }

        try
        {
            var instances = MSBuildLocator
                .QueryVisualStudioInstances()
                .ToList();

            if (instances.Count == 0)
            {
                errorMessage =
                    "No MSBuild instance discovered on this machine. " +
                    "Install the .NET SDK (matching the helper's TargetFramework) " +
                    "or set MSBUILD_EXE_PATH.";
                return false;
            }

            // Prefer .NET SDK instances over Visual Studio instances.
            // The VS BuildHost (net472) crashes with TypeInitializationException
            // when loading solutions that contain certain project types — so the
            // VS instance is only chosen when no SDK instance can even target
            // the repo (certain NETSDK1045 load failure either way).
            var requiredMajor = MsbuildRestore.RequiredSdkMajorFloor(solutionPath);
            var candidates = instances
                .Select(i => new MsbuildCandidate(i.Version, i.DiscoveryType == DiscoveryType.DotNetSdk))
                .ToList();
            var (index, isSdk) = MsbuildRestore.ChooseMsBuildInstance(candidates, requiredMajor);
            var instance = instances[index];
            if (!isSdk)
            {
                Console.Error.WriteLine(
                    $"[INFO] no CLI .NET SDK instance can target net{requiredMajor} projects of " +
                    $"'{solutionPath}' — registering the VS MSBuild '{instance.Name}' v{instance.Version} " +
                    "instead (VS-bundled SDK targets).");
            }

            Console.Error.WriteLine(
                $"MSBuild: registering '{instance.Name}' v{instance.Version} at {instance.MSBuildPath}");
            MSBuildLocator.RegisterInstance(instance);

            // Set MSBUILD_EXE_PATH so the Roslyn BuildHost uses the same MSBuild
            // binary we just registered, instead of discovering a different (possibly
            // incompatible) one from Visual Studio.
            if (!string.IsNullOrEmpty(instance.MSBuildPath) && File.Exists(instance.MSBuildPath))
            {
                Environment.SetEnvironmentVariable("MSBUILD_EXE_PATH", instance.MSBuildPath);
            }

            errorMessage = null;
            return true;
        }
        catch (Exception ex)
        {
            errorMessage =
                $"Failed to register MSBuild: {ex.GetType().Name}: {ex.Message}{Environment.NewLine}{ex.StackTrace}";
            return false;
        }
    }

    // ── Argument parsers ─────────────────────────────────────────────

    /// <summary>
    /// Reads the next argument value or prints an error and returns null.
    /// Shared by all subcommand parsers to avoid the `if (i+1 >= args.Length)` boilerplate.
    /// </summary>
    private static string? RequireValue(string[] args, ref int i, string flag)
    {
        if (i + 1 >= args.Length)
        {
            Console.Error.WriteLine($"{flag} requires a value");
            return null;
        }
        return args[++i];
    }

    /// <summary>
    /// Reads the next arg value and validates it as a filesystem path.
    ///
    /// SECURITY: All CLI path arguments must go through this helper instead of
    /// <see cref="RequireValue"/>. <see cref="Path.GetFullPath"/> canonicalizes
    /// the path (collapsing "..", resolving relative segments, rejecting
    /// malformed inputs), which prevents path-traversal attacks where a
    /// crafted argument could read or write outside expected directories
    /// (Aikido group 30640677). The .NET helper is invoked by the Rust parent
    /// process; this is defense-in-depth, not the primary boundary.
    /// </summary>
    /// <param name="mustExist">If true, the path must point to an existing file.</param>
    /// <returns>The canonicalized absolute path, or null + stderr message on failure.</returns>
    private static string? RequireValidPath(string[] args, ref int i, string flag, bool mustExist)
    {
        var raw = RequireValue(args, ref i, flag);
        if (raw is null) return null;

        if (string.IsNullOrWhiteSpace(raw))
        {
            Console.Error.WriteLine($"{flag} must not be empty or whitespace");
            return null;
        }

        string full;
        try
        {
            // GetFullPath normalizes separators, resolves relative segments
            // (../..), and rejects malformed inputs. This is the central
            // path-traversal defense for CLI args.
            full = Path.GetFullPath(raw);
        }
        catch (Exception ex) when (ex is ArgumentException or PathTooLongException or NotSupportedException)
        {
            Console.Error.WriteLine($"{flag}: invalid path '{raw}': {ex.Message}");
            return null;
        }

        if (mustExist && !File.Exists(full))
        {
            Console.Error.WriteLine($"{flag}: file not found: {full}");
            return null;
        }

        return full;
    }

    private static (string? SolutionPath, string? ProjectPath, string OutputPath, string? ProjectFilter)?
        ParseIndexArgs(string[] args)
    {
        string? solutionPath = null;
        string? projectPath = null;
        string? outputPath = null;
        string? projectFilter = null;

        for (int i = 0; i < args.Length; i++)
        {
            switch (args[i])
            {
                case "--solution":
                    solutionPath = RequireValidPath(args, ref i, "--solution", mustExist: true);
                    if (solutionPath is null) return null;
                    break;
                case "--project":
                    projectPath = RequireValidPath(args, ref i, "--project", mustExist: true);
                    if (projectPath is null) return null;
                    break;
                case "--output":
                    outputPath = RequireValidPath(args, ref i, "--output", mustExist: false);
                    if (outputPath is null) return null;
                    break;
                case "--filter-project":
                    projectFilter = RequireValue(args, ref i, "--filter-project");
                    if (projectFilter is null) return null;
                    break;
                default:
                    Console.Error.WriteLine($"Unknown argument: {args[i]}");
                    return null;
            }
        }

        if (string.IsNullOrEmpty(outputPath))
        {
            Console.Error.WriteLine("--output is required");
            return null;
        }

        if (string.IsNullOrEmpty(solutionPath) && string.IsNullOrEmpty(projectPath))
        {
            Console.Error.WriteLine("Either --solution or --project must be specified");
            return null;
        }

        return (solutionPath, projectPath, outputPath!, projectFilter);
    }

    private static (string SolutionPath, string Symbol, string OutputPath, string? ProjectFilter)?
        ParseFindRefsArgs(string[] args)
    {
        string? solutionPath = null;
        string? symbol = null;
        string? outputPath = null;
        string? projectFilter = null;

        for (int i = 0; i < args.Length; i++)
        {
            switch (args[i])
            {
                case "--solution":
                    solutionPath = RequireValidPath(args, ref i, "--solution", mustExist: true);
                    if (solutionPath is null) return null;
                    break;
                case "--symbol":
                    symbol = RequireValue(args, ref i, "--symbol");
                    if (symbol is null) return null;
                    break;
                case "--output":
                    outputPath = RequireValidPath(args, ref i, "--output", mustExist: false);
                    if (outputPath is null) return null;
                    break;
                case "--filter-project":
                    projectFilter = RequireValue(args, ref i, "--filter-project");
                    if (projectFilter is null) return null;
                    break;
                default:
                    Console.Error.WriteLine($"Unknown argument: {args[i]}");
                    return null;
            }
        }

        if (string.IsNullOrEmpty(solutionPath)) { Console.Error.WriteLine("find-refs: --solution is required"); return null; }
        if (string.IsNullOrEmpty(symbol)) { Console.Error.WriteLine("find-refs: --symbol is required"); return null; }
        if (string.IsNullOrEmpty(outputPath)) { Console.Error.WriteLine("find-refs: --output is required"); return null; }

        return (solutionPath!, symbol!, outputPath!, projectFilter);
    }

    private static (string SolutionPath, IReadOnlyList<string> Symbols, string OutputPath)?
        ParseBatchFindRefsArgs(string[] args)
    {
        string? solutionPath = null;
        string? symbolsFile = null;
        string? symbolsInline = null;
        string? outputPath = null;

        for (int i = 0; i < args.Length; i++)
        {
            switch (args[i])
            {
                case "--solution":
                    solutionPath = RequireValidPath(args, ref i, "--solution", mustExist: true);
                    if (solutionPath is null) return null;
                    break;
                case "--symbols-file":
                    symbolsFile = RequireValidPath(args, ref i, "--symbols-file", mustExist: true);
                    if (symbolsFile is null) return null;
                    break;
                case "--symbols":
                    symbolsInline = RequireValue(args, ref i, "--symbols");
                    if (symbolsInline is null) return null;
                    break;
                case "--output":
                    outputPath = RequireValidPath(args, ref i, "--output", mustExist: false);
                    if (outputPath is null) return null;
                    break;
                default:
                    Console.Error.WriteLine($"Unknown argument: {args[i]}");
                    return null;
            }
        }

        if (string.IsNullOrEmpty(solutionPath)) { Console.Error.WriteLine("batch-find-refs: --solution is required"); return null; }
        if (string.IsNullOrEmpty(outputPath)) { Console.Error.WriteLine("batch-find-refs: --output is required"); return null; }

        IReadOnlyList<string> symbols;
        if (!string.IsNullOrEmpty(symbolsFile))
        {
            // Existence + canonicalization already enforced by RequireValidPath
            // above (mustExist: true). No redundant File.Exists here.
            symbols = File.ReadAllLines(symbolsFile)
                .Select(l => l.Trim())
                .Where(l => !string.IsNullOrEmpty(l) && !l.StartsWith('#'))
                .ToList();
        }
        else if (!string.IsNullOrEmpty(symbolsInline))
        {
            symbols = symbolsInline.Split(';', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);
        }
        else
        {
            Console.Error.WriteLine("batch-find-refs: --symbols-file or --symbols is required");
            return null;
        }

        if (symbols.Count == 0)
        {
            Console.Error.WriteLine("batch-find-refs: no symbols to resolve");
            return null;
        }

        return (solutionPath!, symbols, outputPath!);
    }

    // ── Usage ────────────────────────────────────────────────────────

    [ExcludeFromCodeCoverage]
    private static void PrintUsage()
    {
        Console.WriteLine("scip-csharp — SCIP symbol indexer for C#");
        Console.WriteLine();
        Console.WriteLine("Usage:");
        Console.WriteLine("  scip-csharp index --solution <path> --output <path> [--filter-project <name>]");
        Console.WriteLine("  scip-csharp index --project <path> --output <path>");
        Console.WriteLine("  scip-csharp find-refs --solution <path> --symbol <scip-key> --output <path>");
        Console.WriteLine("  scip-csharp batch-find-refs --solution <path> --symbols-file <path> --output <path>");
        Console.WriteLine("  scip-csharp batch-find-refs --solution <path> --symbols <key1;key2;...> --output <path>");
        Console.WriteLine("  scip-csharp serve --solution <path>");
        Console.WriteLine();
        Console.WriteLine("Options (index):");
        Console.WriteLine("  --solution <path>         Path to .sln file");
        Console.WriteLine("  --project <path>          Path to .csproj file");
        Console.WriteLine("  --output <path>           Output JSON file path");
        Console.WriteLine("  --filter-project <name>   Only index this project within a solution");
        Console.WriteLine();
        Console.WriteLine("Options (find-refs):");
        Console.WriteLine("  --solution <path>         Path to .sln file");
        Console.WriteLine("  --symbol <scip-key>       SCIP symbol key to resolve references for");
        Console.WriteLine("  --output <path>           Output JSON file path");
        Console.WriteLine("  --filter-project <name>   Limit compilation scope (references still searched globally)");
        Console.WriteLine();
        Console.WriteLine("Options (batch-find-refs):");
        Console.WriteLine("  --solution <path>         Path to .sln file");
        Console.WriteLine("  --symbols-file <path>     File with one SCIP key per line");
        Console.WriteLine("  --symbols <key1;key2>     Semicolon-separated SCIP keys");
        Console.WriteLine("  --output <path>           Output JSON file path");
        Console.WriteLine();
        Console.WriteLine("Options (serve):");
        Console.WriteLine("  --solution <path>         Path to .sln file (loaded once; the process then");
        Console.WriteLine("                            serves find-refs/reload requests as JSON lines on");
        Console.WriteLine("                            stdin/stdout until EOF or a shutdown request)");
    }

    [ExcludeFromCodeCoverage]
    private static async Task<int> UnknownCommand(string cmd)
    {
        await Console.Error.WriteLineAsync($"Unknown command: '{cmd}'. Use 'index', 'find-refs', 'batch-find-refs', or 'serve'.").ConfigureAwait(false);
        return 1;
    }
}
