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
            _ => await UnknownCommand(args[0]).ConfigureAwait(false),
        };
    }

    // ── index subcommand ─────────────────────────────────────────────

    private static async Task<int> RunIndexAsync(string[] args)
    {
        var parsed = ParseIndexArgs(args);
        if (parsed is null) return 1;

        if (!TryRegisterMsBuild(out var regErr)) { await Console.Error.WriteLineAsync(regErr).ConfigureAwait(false); return 1; }

        var workspace = MSBuildWorkspace.Create();
        workspace.WorkspaceFailed += (_, e) =>
            Console.Error.WriteLine($"[WARN] Workspace error: {e.Diagnostic}");

        try
        {
            if (!string.IsNullOrEmpty(parsed.Value.SolutionPath))
            {
                Console.Error.WriteLine($"Loading solution: {parsed.Value.SolutionPath}");
                await workspace.OpenSolutionAsync(parsed.Value.SolutionPath).ConfigureAwait(false);
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
            await Console.Error.WriteLineAsync(
                $"Failed to load: {ex.GetType().Name}: {ex.Message}{Environment.NewLine}{ex.StackTrace}")
                .ConfigureAwait(false);
            return 1;
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

        var workspace = MSBuildWorkspace.Create();
        workspace.WorkspaceFailed += (_, e) =>
            Console.Error.WriteLine($"[WARN] Workspace error: {e.Diagnostic}");

        try
        {
            Console.Error.WriteLine($"find-refs: loading solution: {parsed.Value.SolutionPath}");
            await workspace.OpenSolutionAsync(parsed.Value.SolutionPath).ConfigureAwait(false);
        }
        catch (Exception ex)
        {
            await Console.Error.WriteLineAsync(
                $"find-refs: failed to load solution: {ex.GetType().Name}: {ex.Message}{Environment.NewLine}{ex.StackTrace}")
                .ConfigureAwait(false);
            return 1;
        }

        var resolver = new ReferenceResolver();
        FindRefsOutput result;
        try
        {
            result = await resolver.FindRefsAsync(
                workspace.CurrentSolution,
                parsed.Value.Symbol,
                parsed.Value.ProjectFilter).ConfigureAwait(false);
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

    // ── MSBuild registration ─────────────────────────────────────────

    /// <summary>
    /// Register the highest available MSBuild instance.
    /// Returns true on success; sets <paramref name="errorMessage"/> on failure.
    /// </summary>
    private static bool TryRegisterMsBuild([System.Diagnostics.CodeAnalysis.NotNullWhen(false)] out string? errorMessage)
    {
        try
        {
            var instances = MSBuildLocator
                .QueryVisualStudioInstances()
                .OrderByDescending(i => i.Version)
                .ToList();

            if (instances.Count == 0)
            {
                errorMessage =
                    "No MSBuild instance discovered on this machine. " +
                    "Install the .NET SDK (matching the helper's TargetFramework) " +
                    "or set MSBUILD_EXE_PATH.";
                return false;
            }

            var instance = instances[0];
            Console.Error.WriteLine(
                $"MSBuild: registering '{instance.Name}' v{instance.Version} at {instance.MSBuildPath}");
            MSBuildLocator.RegisterInstance(instance);
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
                    if (i + 1 >= args.Length) { Console.Error.WriteLine("--solution requires a value"); return null; }
                    solutionPath = args[++i];
                    break;
                case "--project":
                    if (i + 1 >= args.Length) { Console.Error.WriteLine("--project requires a value"); return null; }
                    projectPath = args[++i];
                    break;
                case "--output":
                    if (i + 1 >= args.Length) { Console.Error.WriteLine("--output requires a value"); return null; }
                    outputPath = args[++i];
                    break;
                case "--filter-project":
                    if (i + 1 >= args.Length) { Console.Error.WriteLine("--filter-project requires a value"); return null; }
                    projectFilter = args[++i];
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
                    if (i + 1 >= args.Length) { Console.Error.WriteLine("--solution requires a value"); return null; }
                    solutionPath = args[++i];
                    break;
                case "--symbol":
                    if (i + 1 >= args.Length) { Console.Error.WriteLine("--symbol requires a value"); return null; }
                    symbol = args[++i];
                    break;
                case "--output":
                    if (i + 1 >= args.Length) { Console.Error.WriteLine("--output requires a value"); return null; }
                    outputPath = args[++i];
                    break;
                case "--filter-project":
                    if (i + 1 >= args.Length) { Console.Error.WriteLine("--filter-project requires a value"); return null; }
                    projectFilter = args[++i];
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
    }

    [ExcludeFromCodeCoverage]
    private static async Task<int> UnknownCommand(string cmd)
    {
        await Console.Error.WriteLineAsync($"Unknown command: '{cmd}'. Use 'index' or 'find-refs'.").ConfigureAwait(false);
        return 1;
    }
}
