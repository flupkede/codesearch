using System.Text.Json;
using System.Text.Json.Serialization;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.MSBuild;
using Microsoft.CodeAnalysis.Text;

namespace ScipCsharp;

/// <summary>
/// Protocol models for the `serve` subcommand (resident mode).
///
/// Line-based JSON over stdin/stdout; all logging stays on stderr so stdout
/// carries ONLY protocol responses. Requests are processed strictly
/// sequentially — one request at a time per workspace, by design (see todo
/// #115): a reload or shutdown must never race a find-refs.
/// </summary>
public sealed class ServeRequest
{
    /// <summary>"ping" | "find-refs" | "reload" | "update" | "shutdown".</summary>
    public string Op { get; set; } = "";
    /// <summary>SCIP symbol key (find-refs only).</summary>
    public string Symbol { get; set; } = "";
    /// <summary>Solution path (reload only; empty keeps the current one).</summary>
    public string Solution { get; set; } = "";
    /// <summary>Absolute paths created or modified on disk (update only).</summary>
    public string[] Changed { get; set; } = [];
    /// <summary>Absolute paths removed from disk (update only).</summary>
    public string[] Deleted { get; set; } = [];
}

public sealed class ServeResponse
{
    public bool Ok { get; set; }
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Error { get; set; }
    /// <summary>True on the initial ready line, after the workspace is loaded.</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public bool? Ready { get; set; }
    /// <summary>Loaded project count (load/reload responses).</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public int? Projects { get; set; }
    /// <summary>Paths applied to the snapshot (update responses).</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public int? Applied { get; set; }
    /// <summary>find-refs payload — identical shape to the file the
    /// `find-refs` subcommand writes, so the Rust host reuses one model.</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public FindRefsOutput? Result { get; set; }
}

/// <summary>
/// The resident request loop. Owns NOTHING about solution loading — Program
/// loads the workspace (same tolerant pipeline as every subcommand) and hands
/// it over together with a reload callback, so reload semantics stay defined
/// in exactly one place (OpenSolutionFilteredAsync).
///
/// find-refs answers from <c>_current</c>, an immutable snapshot that only
/// reload (re-reads the workspace) and update (in-memory text edits) replace.
/// Nothing here ever writes to disk or applies changes to the workspace.
/// </summary>
public sealed class ServeHost
{
    private static readonly JsonSerializerOptions Options = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
        WriteIndented = false,
    };

    private const string NeedsReloadPrefix = "needs-reload: ";
    private const string SourceExtension = ".cs";

    // A change to any of these alters the project graph or its references,
    // which a text edit of the snapshot cannot express.
    private static readonly string[] ReloadExtensions = [".csproj", ".sln", ".slnx", ".props", ".targets"];
    private static readonly string[] ReloadFileNames = ["packages.lock.json", "global.json"];

    private static readonly StringComparison PathComparison = OperatingSystem.IsWindows()
        ? StringComparison.OrdinalIgnoreCase
        : StringComparison.Ordinal;
    private static readonly StringComparer PathComparer = StringComparer.FromComparison(PathComparison);

    private readonly MSBuildWorkspace _workspace;
    private readonly Func<string, Task> _reloadSolution;
    private readonly string? _defaultSolution;
    private readonly ReferenceResolver _resolver = new();
    private Solution _current;

    internal ServeHost(MSBuildWorkspace workspace, Func<string, Task> reloadSolution, string? defaultSolution)
    {
        _workspace = workspace;
        _reloadSolution = reloadSolution;
        _defaultSolution = defaultSolution;
        _current = workspace.CurrentSolution;
    }

    /// <summary>
    /// Runs until stdin EOF or a "shutdown" request. Returns 0 on both —
    /// the Rust side kills the process for teardown, which is the only
    /// reliable way to dispose a Roslyn workspace.
    /// </summary>
    public static async Task<int> LoopAsync(
        MSBuildWorkspace workspace,
        Func<string, Task> reloadSolution,
        string? defaultSolution)
    {
        var host = new ServeHost(workspace, reloadSolution, defaultSolution);

        // Handshake: the Rust host waits for this line before sending any
        // request — workspace load takes minutes and must not look like a
        // hung helper.
        await RespondAsync(new ServeResponse { Ok = true, Ready = true, Projects = host._current.Projects.Count() })
            .ConfigureAwait(false);

        string? line;
        while ((line = await Console.In.ReadLineAsync().ConfigureAwait(false)) is not null)
        {
            var response = await host.HandleLineAsync(line).ConfigureAwait(false);
            if (response is null)
            {
                // shutdown — acknowledge, then exit the loop normally.
                await RespondAsync(new ServeResponse { Ok = true }).ConfigureAwait(false);
                return 0;
            }
            await RespondAsync(response).ConfigureAwait(false);
        }

        // stdin EOF: the host closed the pipe (kill/teardown path) — exit 0.
        return 0;
    }

    /// <summary>Dispatch one request line. Returns null for "shutdown".</summary>
    internal async Task<ServeResponse?> HandleLineAsync(string line)
    {
        ServeRequest? request;
        try
        {
            request = JsonSerializer.Deserialize<ServeRequest>(line, Options);
        }
        catch (JsonException ex)
        {
            return new ServeResponse { Ok = false, Error = $"bad request: {ex.Message}" };
        }

        if (request is null || string.IsNullOrEmpty(request.Op))
        {
            return new ServeResponse { Ok = false, Error = "missing op" };
        }

        try
        {
            switch (request.Op)
            {
                case "ping":
                    return new ServeResponse { Ok = true, Projects = _workspace.CurrentSolution.Projects.Count() };
                case "find-refs":
                    return await HandleFindRefsAsync(request.Symbol).ConfigureAwait(false);
                case "reload":
                    return await HandleReloadAsync(request.Solution).ConfigureAwait(false);
                case "update":
                    return HandleUpdate(request.Changed, request.Deleted);
                case "shutdown":
                    return null;
                default:
                    return new ServeResponse { Ok = false, Error = $"unknown op '{request.Op}'" };
            }
        }
        catch (Exception ex)
        {
            // One failed request must never kill the loop — the host decides
            // lifecycle (kill on teardown), the helper answers on failures.
            return new ServeResponse { Ok = false, Error = $"{ex.GetType().Name}: {ex.Message}" };
        }
    }

    private async Task<ServeResponse> HandleFindRefsAsync(string symbol)
    {
        if (string.IsNullOrWhiteSpace(symbol))
        {
            return new ServeResponse { Ok = false, Error = "find-refs requires symbol" };
        }
        var result = await _resolver.FindRefsAsync(_current, symbol).ConfigureAwait(false);
        return new ServeResponse { Ok = true, Result = result };
    }

    private async Task<ServeResponse> HandleReloadAsync(string solution)
    {
        _workspace.CloseSolution();
        try
        {
            var target = string.IsNullOrWhiteSpace(solution) ? _defaultSolution : solution;
            if (string.IsNullOrWhiteSpace(target))
            {
                _workspace.CloseSolution();
                return new ServeResponse { Ok = false, Error = "reload requires solution (none loaded)" };
            }
            await _reloadSolution(target).ConfigureAwait(false);
            return new ServeResponse { Ok = true, Projects = _workspace.CurrentSolution.Projects.Count() };
        }
        finally
        {
            // Whatever the workspace holds after the attempt, even a partial
            // load or an empty solution, is what find-refs must answer from.
            _current = _workspace.CurrentSolution;
        }
    }

    /// <summary>
    /// Applies on-disk file changes to the held snapshot in place. The new
    /// snapshot is built aside and swapped in only when every path succeeds,
    /// so a failure leaves <c>_current</c> exactly as it was.
    /// </summary>
    private ServeResponse HandleUpdate(string[]? changedInput, string[]? deletedInput)
    {
        var processing = string.Empty;
        try
        {
            var changed = new List<string>();
            foreach (var raw in changedInput ?? [])
            {
                processing = raw;
                changed.Add(PathGuard.Validate(raw));
            }
            var deleted = new List<string>();
            foreach (var raw in deletedInput ?? [])
            {
                processing = raw;
                deleted.Add(PathGuard.Validate(raw));
            }
            changed = changed.Distinct(PathComparer).ToList();
            deleted = deleted.Distinct(PathComparer).ToList();

            foreach (var path in changed.Concat(deleted))
            {
                processing = path;
                if (RequiresReload(path))
                {
                    return NeedsReload(path);
                }
            }

            var next = _current;
            var applied = 0;
            foreach (var path in changed)
            {
                processing = path;
                var ids = next.GetDocumentIdsWithFilePath(path);
                if (ids.Length > 0)
                {
                    var text = SourceText.From(File.ReadAllText(path));
                    foreach (var id in ids)
                    {
                        next = WithText(next, id, text);
                    }
                    applied++;
                }
                else if (path.EndsWith(SourceExtension, StringComparison.OrdinalIgnoreCase))
                {
                    var owners = OwningProjects(next, path);
                    if (owners.Count == 0)
                    {
                        return NeedsReload(path);
                    }
                    var text = SourceText.From(File.ReadAllText(path));
                    foreach (var owner in owners)
                    {
                        next = next.AddDocument(
                            DocumentId.CreateNewId(owner.Id), Path.GetFileName(path), text, filePath: path);
                    }
                    applied++;
                }
            }
            foreach (var path in deleted)
            {
                processing = path;
                var ids = next.GetDocumentIdsWithFilePath(path);
                if (ids.Length == 0)
                {
                    continue;
                }
                foreach (var id in ids)
                {
                    next = Without(next, id);
                }
                applied++;
            }

            _current = next;
            Console.Error.WriteLine($"update: applied {applied} path(s)");
            return new ServeResponse { Ok = true, Applied = applied };
        }
        catch (Exception ex)
        {
            Console.Error.WriteLine($"[WARN] update: {ex.GetType().Name}: {ex.Message}");
            return NeedsReload(processing);
        }
    }

    private static ServeResponse NeedsReload(string path) =>
        new() { Ok = false, Error = NeedsReloadPrefix + path };

    private static bool RequiresReload(string path)
    {
        var name = Path.GetFileName(path);
        return ReloadExtensions.Any(e => name.EndsWith(e, StringComparison.OrdinalIgnoreCase))
            || ReloadFileNames.Any(n => name.Equals(n, StringComparison.OrdinalIgnoreCase));
    }

    /// <summary>
    /// Projects whose directory is the deepest one containing
    /// <paramref name="path"/>. Several projects share it for a multi-target
    /// project (one entry per target framework); the file belongs to each.
    /// </summary>
    private static List<Project> OwningProjects(Solution solution, string path)
    {
        var owners = new List<(Project Project, string Directory)>();
        foreach (var project in solution.Projects)
        {
            if (project.FilePath is null)
            {
                continue;
            }
            var directory = Path.GetDirectoryName(Path.GetFullPath(project.FilePath));
            if (directory is null)
            {
                continue;
            }
            directory = directory.TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar);
            if (path.StartsWith(directory + Path.DirectorySeparatorChar, PathComparison))
            {
                owners.Add((project, directory));
            }
        }
        if (owners.Count == 0)
        {
            return [];
        }
        var deepest = owners.Max(o => o.Directory.Length);
        return owners.Where(o => o.Directory.Length == deepest).Select(o => o.Project).ToList();
    }

    // A file path can also be an additional document (.razor) or an analyzer
    // config (.editorconfig); each kind has its own text/remove API.
    private static Solution WithText(Solution solution, DocumentId id, SourceText text) =>
        solution.ContainsDocument(id) ? solution.WithDocumentText(id, text)
        : solution.ContainsAdditionalDocument(id) ? solution.WithAdditionalDocumentText(id, text)
        : solution.WithAnalyzerConfigDocumentText(id, text);

    private static Solution Without(Solution solution, DocumentId id) =>
        solution.ContainsDocument(id) ? solution.RemoveDocument(id)
        : solution.ContainsAdditionalDocument(id) ? solution.RemoveAdditionalDocument(id)
        : solution.RemoveAnalyzerConfigDocument(id);

    internal static string ToJson(ServeResponse response) => JsonSerializer.Serialize(response, Options);

    private static async Task RespondAsync(ServeResponse response)
    {
        await Console.Out.WriteLineAsync(ToJson(response)).ConfigureAwait(false);
        await Console.Out.FlushAsync().ConfigureAwait(false);
    }
}
