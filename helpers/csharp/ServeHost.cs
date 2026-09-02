using System.Text.Json;
using System.Text.Json.Serialization;
using Microsoft.CodeAnalysis.MSBuild;

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
    /// <summary>"ping" | "find-refs" | "reload" | "shutdown".</summary>
    public string Op { get; set; } = "";
    /// <summary>SCIP symbol key (find-refs only).</summary>
    public string Symbol { get; set; } = "";
    /// <summary>Solution path (reload only; empty keeps the current one).</summary>
    public string Solution { get; set; } = "";
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
/// </summary>
public static class ServeHost
{
    private static readonly JsonSerializerOptions Options = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
        WriteIndented = false,
    };

    /// <summary>
    /// Runs until stdin EOF or a "shutdown" request. Returns 0 on both —
    /// the Rust side kills the process for teardown, which is the only
    /// reliable way to dispose a Roslyn workspace.
    /// </summary>
    public static async Task<int> LoopAsync(
        MSBuildWorkspace workspace,
        Func<string, Task> reloadSolution)
    {
        var resolver = new ReferenceResolver();

        // Handshake: the Rust host waits for this line before sending any
        // request — workspace load takes minutes and must not look like a
        // hung helper.
        await RespondAsync(new ServeResponse { Ok = true, Ready = true, Projects = workspace.CurrentSolution.Projects.Count() })
            .ConfigureAwait(false);

        string? line;
        while ((line = await Console.In.ReadLineAsync().ConfigureAwait(false)) is not null)
        {
            var response = await HandleLineAsync(workspace, reloadSolution, resolver, line).ConfigureAwait(false);
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
    private static async Task<ServeResponse?> HandleLineAsync(
        MSBuildWorkspace workspace,
        Func<string, Task> reloadSolution,
        ReferenceResolver resolver,
        string line)
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
                    return new ServeResponse { Ok = true, Projects = workspace.CurrentSolution.Projects.Count() };
                case "find-refs":
                    return await HandleFindRefsAsync(resolver, workspace, request.Symbol).ConfigureAwait(false);
                case "reload":
                    return await HandleReloadAsync(workspace, reloadSolution, request.Solution).ConfigureAwait(false);
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

    private static async Task<ServeResponse> HandleFindRefsAsync(
        ReferenceResolver resolver, MSBuildWorkspace workspace, string symbol)
    {
        if (string.IsNullOrWhiteSpace(symbol))
        {
            return new ServeResponse { Ok = false, Error = "find-refs requires symbol" };
        }
        var result = await resolver.FindRefsAsync(workspace.CurrentSolution, symbol).ConfigureAwait(false);
        return new ServeResponse { Ok = true, Result = result };
    }

    private static async Task<ServeResponse> HandleReloadAsync(
        MSBuildWorkspace workspace, Func<string, Task> reloadSolution, string solution)
    {
        workspace.CloseSolution();
        var target = string.IsNullOrWhiteSpace(solution) ? Program.CurrentServeSolution : solution;
        if (string.IsNullOrWhiteSpace(target))
        {
            workspace.CloseSolution();
            return new ServeResponse { Ok = false, Error = "reload requires solution (none loaded)" };
        }
        await reloadSolution(target).ConfigureAwait(false);
        return new ServeResponse { Ok = true, Projects = workspace.CurrentSolution.Projects.Count() };
    }

    private static async Task RespondAsync(ServeResponse response)
    {
        await Console.Out.WriteLineAsync(JsonSerializer.Serialize(response, Options)).ConfigureAwait(false);
        await Console.Out.FlushAsync().ConfigureAwait(false);
    }
}
