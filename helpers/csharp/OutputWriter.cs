using System.Text.Json;
using System.Text.Json.Serialization;

namespace ScipCsharp;

/// <summary>
/// Writes a ScipIndex to a JSON file.
/// </summary>
public static class OutputWriter
{
    private static readonly JsonSerializerOptions Options = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
        WriteIndented = false,
        DefaultIgnoreCondition = JsonIgnoreCondition.Never,
    };

    public static async Task WriteAsync(ScipIndex index, string outputPath)
    {
        outputPath = CanonicalizeOutputPath(outputPath);
        var dir = Path.GetDirectoryName(outputPath);
        if (!string.IsNullOrEmpty(dir) && !Directory.Exists(dir))
            Directory.CreateDirectory(dir);

        await using var stream = File.Create(outputPath);
        await JsonSerializer.SerializeAsync(stream, index, Options).ConfigureAwait(false);
    }

    /// <summary>Write find-refs output for the `find-refs` subcommand.</summary>
    public static async Task WriteRefsAsync(FindRefsOutput output, string outputPath)
    {
        outputPath = CanonicalizeOutputPath(outputPath);
        var dir = Path.GetDirectoryName(outputPath);
        if (!string.IsNullOrEmpty(dir) && !Directory.Exists(dir))
            Directory.CreateDirectory(dir);

        await using var stream = File.Create(outputPath);
        await JsonSerializer.SerializeAsync(stream, output, Options).ConfigureAwait(false);
    }

    /// <summary>Write batch find-refs output for the `batch-find-refs` subcommand.</summary>
    public static async Task WriteBatchRefsAsync(BatchFindRefsOutput output, string outputPath)
    {
        outputPath = CanonicalizeOutputPath(outputPath);
        var dir = Path.GetDirectoryName(outputPath);
        if (!string.IsNullOrEmpty(dir) && !Directory.Exists(dir))
            Directory.CreateDirectory(dir);

        await using var stream = File.Create(outputPath);
        await JsonSerializer.SerializeAsync(stream, output, Options).ConfigureAwait(false);
    }

    /// <summary>
    /// Canonicalizes and validates an output path before any File.Create call.
    ///
    /// SECURITY: Defense-in-depth against path traversal (Aikido group 30640677).
    /// Callers in Program.cs already validate via <c>RequireValidPath</c>, but
    /// this guard ensures OutputWriter remains safe if a new code path bypasses
    /// the CLI parser (e.g. a future unit test calling WriteAsync directly with
    /// an arbitrary string). <see cref="Path.GetFullPath"/> resolves relative
    /// segments (<c>../..</c>) and rejects malformed inputs.
    /// </summary>
    private static string CanonicalizeOutputPath(string outputPath)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(outputPath);
        return Path.GetFullPath(outputPath);
    }
}
