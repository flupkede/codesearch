using System.Diagnostics.CodeAnalysis;

namespace ScipCsharp;

/// <summary>
/// CLI entrypoint for scip-csharp.
/// Usage: scip-csharp index --solution &lt;path&gt; --output &lt;path&gt; [--filter-project &lt;path&gt;]
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

        if (args[0] != "index")
        {
            await Console.Error.WriteLineAsync($"Unknown command: {args[0]}. Expected 'index'.").ConfigureAwait(false);
            return 1;
        }

        var parsed = ParseArgs(args[1..]);
        if (parsed is null)
            return 1;

        // Register MSBuild before any workspace operations
        Microsoft.Build.Locator.MSBuildLocator.RegisterDefaults();

        var workspace = Microsoft.CodeAnalysis.MSBuild.MSBuildWorkspace.Create();
        workspace.WorkspaceFailed += (_, e) =>
        {
            Console.Error.WriteLine($"[WARN] Workspace error: {e.Diagnostic}");
        };

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
            await Console.Error.WriteLineAsync($"Failed to load: {ex.Message}").ConfigureAwait(false);
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

    private static (string? SolutionPath, string? ProjectPath, string OutputPath, string? ProjectFilter)? ParseArgs(string[] args)
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

        return (solutionPath, projectPath, outputPath, projectFilter);
    }

    [ExcludeFromCodeCoverage]
    private static void PrintUsage()
    {
        Console.WriteLine("scip-csharp — SCIP symbol indexer for C#");
        Console.WriteLine();
        Console.WriteLine("Usage:");
        Console.WriteLine("  scip-csharp index --solution <path> --output <path> [--filter-project <path>]");
        Console.WriteLine("  scip-csharp index --project <path> --output <path>");
        Console.WriteLine();
        Console.WriteLine("Options:");
        Console.WriteLine("  --solution <path>         Path to .sln file");
        Console.WriteLine("  --project <path>          Path to .csproj file");
        Console.WriteLine("  --output <path>           Output JSON file path");
        Console.WriteLine("  --filter-project <path>   Only index this project within a solution");
    }
}
