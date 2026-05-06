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
        var dir = Path.GetDirectoryName(outputPath);
        if (!string.IsNullOrEmpty(dir) && !Directory.Exists(dir))
            Directory.CreateDirectory(dir);

        await using var stream = File.Create(outputPath);
        await JsonSerializer.SerializeAsync(stream, index, Options).ConfigureAwait(false);
    }

    /// <summary>Write find-refs output for the `find-refs` subcommand.</summary>
    public static async Task WriteRefsAsync(FindRefsOutput output, string outputPath)
    {
        var dir = Path.GetDirectoryName(outputPath);
        if (!string.IsNullOrEmpty(dir) && !Directory.Exists(dir))
            Directory.CreateDirectory(dir);

        await using var stream = File.Create(outputPath);
        await JsonSerializer.SerializeAsync(stream, output, Options).ConfigureAwait(false);
    }
}
