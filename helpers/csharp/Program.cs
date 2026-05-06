using System.Diagnostics.CodeAnalysis;
using Microsoft.Build.Locator;
using Microsoft.CodeAnalysis.MSBuild;

namespace ScipCsharp;

/// <summary>
/// CLI entrypoint for scip-csharp.
///
/// Subcommands:
///   index     — compile solution, collect definitions, write SCIP JSON (fast, no FindReferencesAsync)
///   find-refs — resolve references for a single symbol on demand (for lazy find_impact caching)
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

        return args[0] switch
        {
            "index" => await RunIndexAsync(args[1..]).ConfigureAwait(false),
            "find-refs" => await RunFindRefsAsync(args[1..]).ConfigureAwait(false),
            "batch-find-refs" => await RunBatchFindRefsAsync(args[1..]).ConfigureAwait(false),
            _ => await UnknownCommand(args[0]).ConfigureAwait(false),
        };
    }

    // ── index subcommand ─────────────────────────────────────────────

    private static async Task<int> RunIndexAsync(string[] args)
    {
        var parsed = ParseIndexArgs(args);
        if (parsed is null) return 1;

        if (!TryRegisterMsBuild(out var regErr)) { await Console.Error.WriteLineAsync(regErr).ConfigureAwait(false); return 1; }

        using var workspace = CreateTolerantWorkspace();

        try
        {
            if (!string.IsNullOrEmpty(parsed.Value.SolutionPath))
            {
                Console.Error.WriteLine($"Loading solution: {parsed.Value.SolutionPath}");
                await OpenSolutionFilteredAsync(workspace, parsed.Value.SolutionPath).ConfigureAwait(false);
            }
            else if (!string.IsNullOrEmpty(parsed.Value.ProjectPath))
            {
                Console.Error.WriteLine($"Loading project: {parsed.Value.ProjectPath}");
                await workspace.OpenProjectAsync(parsed.Value.ProjectPath).ConfigureAwait(false);
            }
            else
            {
                await Console.Error.WriteLineAsync("Either --solution or --project must be specified.").ConfigureAwait(false);
                return 1;
            }
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

        var indexer = new SymbolIndexer();
        var index = await indexer.IndexAsync(workspace, parsed.Value.ProjectFilter).ConfigureAwait(false);

        await OutputWriter.WriteAsync(index, parsed.Value.OutputPath).ConfigureAwait(false);

        Console.Error.WriteLine($"Index written to: {parsed.Value.OutputPath}");
        Console.Error.WriteLine($"  Documents: {index.Documents.Count}");
        Console.Error.WriteLine($"  Occurrences: {index.Documents.Sum(d => d.Occurrences.Count)}");
        Console.Error.WriteLine($"  Symbols: {index.ExternalSymbols.Count}");

        return 0;
    }

    // ── find-refs subcommand ─────────────────────────────────────────

    private static async Task<int> RunFindRefsAsync(string[] args)
    {
        var parsed = ParseFindRefsArgs(args);
        if (parsed is null) return 1;

        if (!TryRegisterMsBuild(out var regErr)) { await Console.Error.WriteLineAsync(regErr).ConfigureAwait(false); return 1; }

        using var workspace = CreateTolerantWorkspace();

        try
        {
            Console.Error.WriteLine($"find-refs: loading solution: {parsed.Value.SolutionPath}");
            await OpenSolutionFilteredAsync(workspace, parsed.Value.SolutionPath).ConfigureAwait(false);
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

        if (!TryRegisterMsBuild(out var regErr)) { await Console.Error.WriteLineAsync(regErr).ConfigureAwait(false); return 1; }

        using var workspace = CreateTolerantWorkspace();

        try
        {
            Console.Error.WriteLine($"batch-find-refs: loading solution: {parsed.Value.SolutionPath}");
            await OpenSolutionFilteredAsync(workspace, parsed.Value.SolutionPath).ConfigureAwait(false);
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
        var properties = new Dictionary<string, string>
        {
            // Tell Roslyn to skip projects it cannot load instead of crashing.
            { "BuildingInsideVisualStudio", "true" },
        };

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
            Console.Error.WriteLine($"[WARN] Workspace error: {e.Diagnostic}");
        return workspace;
    }

    // ── Solution filtering ───────────────────────────────────────────

    /// <summary>
    /// Opens a solution but first checks if it contains unsupported project types.
    /// If unsupported projects are found, creates a filtered temporary .sln that
    /// excludes them, then opens that instead.
    /// If the filtered solution still fails to load (e.g. BuildHost crash), falls
    /// back to loading individual .csproj files from the solution.
    /// </summary>
    private static async Task OpenSolutionFilteredAsync(MSBuildWorkspace workspace, string solutionPath)
    {
        var solutionDir = Path.GetDirectoryName(solutionPath) ?? ".";
        var lines = await File.ReadAllLinesAsync(solutionPath).ConfigureAwait(false);

        // Parse project entries from .sln: Project("{GUID}") = "Name", "RelativePath", "{GUID}"
        var projectEntries = ParseSolutionProjects(lines);

        // Separate supported vs unsupported projects
        var supported = projectEntries
            .Where(p => !UnsupportedProjectExtensions.Contains(Path.GetExtension(p.RelativePath)))
            .ToList();
        var unsupported = projectEntries
            .Where(p => UnsupportedProjectExtensions.Contains(Path.GetExtension(p.RelativePath)))
            .ToList();

        // Log skipped projects
        foreach (var skip in unsupported)
        {
            Console.Error.WriteLine($"[INFO] Skipping unsupported project type: {skip.RelativePath}");
        }

        if (supported.Count == 0)
        {
            throw new InvalidOperationException(
                $"Solution contains no supported project types " +
                $"(found {unsupported.Count} unsupported: {string.Join(", ", unsupported.Select(u => u.RelativePath))})");
        }

        // Try opening the filtered solution first
        var skipIndices = unsupported.Select(u => u.LineIndex).ToHashSet();
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
            Console.Error.WriteLine(
                $"[WARN] Filtered solution still failed ({ex.GetType().Name}: {ex.Message}); " +
                $"falling back to loading {supported.Count} individual .csproj files.");
        }
        finally
        {
            try { File.Delete(tempSlnPath); } catch { /* best effort */ }
        }

        // Fallback: open each supported .csproj individually.
        // CloseSolution() clears any partially-loaded projects from the failed solution attempt.
        workspace.CloseSolution();
        var loaded = 0;
        foreach (var entry in supported)
        {
            var csprojPath = Path.GetFullPath(Path.Combine(solutionDir, entry.RelativePath));
            if (!File.Exists(csprojPath))
            {
                Console.Error.WriteLine($"[WARN] Project file not found: {csprojPath}");
                continue;
            }

            try
            {
                await workspace.OpenProjectAsync(csprojPath).ConfigureAwait(false);
                loaded++;
            }
            catch (Exception ex)
            {
                Console.Error.WriteLine(
                    $"[WARN] Failed to load project '{entry.RelativePath}': {ex.GetType().Name}: {ex.Message}");
            }
        }

        Console.Error.WriteLine($"Loaded {loaded}/{supported.Count} project(s) individually");
    }

    /// <summary>
    /// Parses project entries from .sln file lines.
    /// Skips solution folders (GUID {2150E333-...}) which have no file path.
    /// </summary>
    private static List<(int LineIndex, string RelativePath)> ParseSolutionProjects(string[] lines)
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

            result.Add((i, relativePath));
        }
        return result;
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
    /// Register the highest available MSBuild instance.
    /// Returns true on success; sets <paramref name="errorMessage"/> on failure.
    /// </summary>
    private static bool TryRegisterMsBuild([System.Diagnostics.CodeAnalysis.NotNullWhen(false)] out string? errorMessage)
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
            // when loading solutions that contain certain project types.
            var sdkInstance = instances
                .FirstOrDefault(i => i.DiscoveryType == DiscoveryType.DotNetSdk);
            var instance = sdkInstance ?? instances.OrderByDescending(i => i.Version).First();

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
                    solutionPath = RequireValue(args, ref i, "--solution");
                    if (solutionPath is null) return null;
                    break;
                case "--project":
                    projectPath = RequireValue(args, ref i, "--project");
                    if (projectPath is null) return null;
                    break;
                case "--output":
                    outputPath = RequireValue(args, ref i, "--output");
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
                    solutionPath = RequireValue(args, ref i, "--solution");
                    if (solutionPath is null) return null;
                    break;
                case "--symbol":
                    symbol = RequireValue(args, ref i, "--symbol");
                    if (symbol is null) return null;
                    break;
                case "--output":
                    outputPath = RequireValue(args, ref i, "--output");
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
                    solutionPath = RequireValue(args, ref i, "--solution");
                    if (solutionPath is null) return null;
                    break;
                case "--symbols-file":
                    symbolsFile = RequireValue(args, ref i, "--symbols-file");
                    if (symbolsFile is null) return null;
                    break;
                case "--symbols":
                    symbolsInline = RequireValue(args, ref i, "--symbols");
                    if (symbolsInline is null) return null;
                    break;
                case "--output":
                    outputPath = RequireValue(args, ref i, "--output");
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
            if (!File.Exists(symbolsFile))
            {
                Console.Error.WriteLine($"batch-find-refs: symbols file not found: {symbolsFile}");
                return null;
            }
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
    }

    [ExcludeFromCodeCoverage]
    private static async Task<int> UnknownCommand(string cmd)
    {
        await Console.Error.WriteLineAsync($"Unknown command: '{cmd}'. Use 'index' or 'find-refs'.").ConfigureAwait(false);
        return 1;
    }
}
