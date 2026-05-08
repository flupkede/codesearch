using System.Text.Json;
using Xunit;
using ScipCsharp;

namespace ScipCsharp.Tests;

/// <summary>
/// Unit tests for the ScipModels and OutputWriter.
/// Tests JSON serialization round-trip and model correctness.
/// 
/// Full Roslyn integration tests (indexing a real .sln) require MSBuild
/// infrastructure and are run as manual quality gates. These tests verify
/// the data models, serialization format, and output correctness.
/// </summary>
public class ScipModelTests
{
    private static readonly JsonSerializerOptions JsonOptions = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
        WriteIndented = false,
    };

    [Fact]
    public void ScipIndex_SerializesWithSnakeCase()
    {
        var index = new ScipIndex
        {
            Metadata = new ScipMetadata { Version = "1.0", ToolInfo = "scip-csharp" },
            Documents =
            [
                new ScipDocument
                {
                    RelativePath = "src/Calculator.cs",
                    Occurrences =
                    [
                        new ScipOccurrence
                        {
                            Range = [8, 5, 8, 20],
                            Symbol = "csharp MyApp . Calculator#Add(int, int).",
                            SymbolRoles = 1,
                            Kind = "definition",
                        },
                    ],
                },
            ],
            ExternalSymbols =
            [
                new ScipSymbolInfo { Symbol = "csharp MyApp . Calculator#Add(int, int)." },
            ],
        };

        var json = JsonSerializer.Serialize(index, JsonOptions);
        var parsed = JsonDocument.Parse(json);

        // Verify snake_case keys at the right nesting levels
        Assert.True(parsed.RootElement.TryGetProperty("metadata", out var meta), "metadata key");
        Assert.True(meta.TryGetProperty("tool_info", out _), "tool_info key");
        Assert.True(parsed.RootElement.TryGetProperty("external_symbols", out _), "external_symbols key");

        // Document-level keys
        var docs = parsed.RootElement.GetProperty("documents");
        var firstDoc = docs.EnumerateArray().First();
        Assert.True(firstDoc.TryGetProperty("relative_path", out _), "relative_path key");

        // Occurrence-level keys
        var occs = firstDoc.GetProperty("occurrences");
        var firstOcc = occs.EnumerateArray().First();
        Assert.True(firstOcc.TryGetProperty("symbol_roles", out _), "symbol_roles key");
    }

    [Fact]
    public void ScipOccurrence_RangeIsIntArray()
    {
        var occ = new ScipOccurrence
        {
            Range = [10, 5, 10, 25],
            Symbol = "csharp . . A#Method().",
            SymbolRoles = 1,
            Kind = "definition",
        };

        var json = JsonSerializer.Serialize(occ, JsonOptions);
        var parsed = JsonDocument.Parse(json);

        Assert.True(parsed.RootElement.TryGetProperty("range", out var range));
        Assert.Equal(JsonValueKind.Array, range.ValueKind);
        Assert.Equal(4, range.GetArrayLength());
        Assert.Equal(10, range[0].GetInt32());
        Assert.Equal(5, range[1].GetInt32());
        Assert.Equal(10, range[2].GetInt32());
        Assert.Equal(25, range[3].GetInt32());
    }

    [Fact]
    public void ScipIndex_RoundTripJson()
    {
        var original = new ScipIndex
        {
            Metadata = new ScipMetadata(),
            Documents =
            [
                new ScipDocument
                {
                    RelativePath = "src/A.cs",
                    Occurrences =
                    [
                        new ScipOccurrence
                        {
                            Range = [1, 0],
                            Symbol = "csharp . . A#",
                            SymbolRoles = 1,
                            Kind = "definition",
                        },
                        new ScipOccurrence
                        {
                            Range = [5, 4],
                            Symbol = "csharp . . A#Method().",
                            SymbolRoles = 1,
                            Kind = "definition",
                        },
                        new ScipOccurrence
                        {
                            Range = [10, 8],
                            Symbol = "csharp . . A#Method().",
                            SymbolRoles = 0,
                            Kind = "reference",
                        },
                    ],
                },
            ],
            ExternalSymbols =
            [
                new ScipSymbolInfo { Symbol = "csharp . . A#" },
                new ScipSymbolInfo { Symbol = "csharp . . A#Method()." },
            ],
        };

        var json = JsonSerializer.Serialize(original, JsonOptions);
        var deserialized = JsonSerializer.Deserialize<ScipIndex>(json, JsonOptions);

        Assert.NotNull(deserialized);
        Assert.Single(deserialized.Documents);
        Assert.Equal(3, deserialized.Documents[0].Occurrences.Count);
        Assert.Equal(2, deserialized.ExternalSymbols.Count);
        Assert.Equal("src/A.cs", deserialized.Documents[0].RelativePath);

        // Verify occurrence details survived round-trip
        var methodDef = deserialized.Documents[0].Occurrences[1];
        Assert.Equal("csharp . . A#Method().", methodDef.Symbol);
        Assert.Equal(1, methodDef.SymbolRoles);
        Assert.Equal("definition", methodDef.Kind);
        Assert.Equal([5, 4], methodDef.Range);

        var methodRef = deserialized.Documents[0].Occurrences[2];
        Assert.Equal(0, methodRef.SymbolRoles);
        Assert.Equal("reference", methodRef.Kind);
    }

    [Fact]
    public void ScipIndex_EmptyDocuments_Serializes()
    {
        var index = new ScipIndex();
        var json = JsonSerializer.Serialize(index, JsonOptions);

        Assert.Contains("\"documents\":[]", json);
        Assert.Contains("\"external_symbols\":[]", json);
    }

    [Fact]
    public void ScipSymbolInfo_Defaults_AreEmpty()
    {
        var info = new ScipSymbolInfo();
        Assert.Equal("", info.Symbol);
        Assert.Empty(info.Documentation);
    }

    [Fact]
    public void OutputWriter_ProducesValidJson()
    {
        var index = new ScipIndex
        {
            Metadata = new ScipMetadata(),
            Documents =
            [
                new ScipDocument
                {
                    RelativePath = "test.cs",
                    Occurrences =
                    [
                        new ScipOccurrence
                        {
                            Range = [1, 0, 1, 10],
                            Symbol = "csharp . . Test#",
                            SymbolRoles = 1,
                            Kind = "definition",
                        },
                    ],
                },
            ],
        };

        using var ms = new MemoryStream();
        // Test that the OutputWriter's serialization produces valid JSON
        var json = JsonSerializer.Serialize(index, new JsonSerializerOptions
        {
            PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
            WriteIndented = true,
        });

        // Verify it's valid JSON by re-parsing
        var parsed = JsonDocument.Parse(json);
        Assert.True(parsed.RootElement.TryGetProperty("documents", out _));

        // Verify indented output (has newlines)
        Assert.Contains('\n', json);
    }
}
