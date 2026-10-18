using System.Text.Json;
using Xunit;
using ScipCsharp;

namespace ScipCsharp.Tests;

/// <summary>
/// The `index` JSON the Rust ingest reads: version "2.1", a top-level
/// `references` list on a full run, and `references: null` when
/// `--filter-project` skipped the reference pass.
/// </summary>
[Collection("msbuild-workspace")]
public class IndexWireContractTests
{
    [Fact]
    public async Task ReferenceRecords_SerializeSnakeCaseUnderTopLevelReferences()
    {
        var index = new ScipIndex
        {
            References =
            [
                new ScipReferenceRecord { Symbol = "csharp App . Calc#Add().", File = "src/Calc.cs", StartLine = 7, EndLine = 8 },
            ],
        };

        using var parsed = await WriteAndParseAsync(index);

        Assert.Equal("2.1", parsed.RootElement.GetProperty("metadata").GetProperty("version").GetString());
        var record = parsed.RootElement.GetProperty("references")[0];
        Assert.Equal("csharp App . Calc#Add().", record.GetProperty("symbol").GetString());
        Assert.Equal("src/Calc.cs", record.GetProperty("file").GetString());
        Assert.Equal(7, record.GetProperty("start_line").GetInt32());
        Assert.Equal(8, record.GetProperty("end_line").GetInt32());
        Assert.Equal("reference", record.GetProperty("kind").GetString());
    }

    [Fact]
    public async Task FilterProjectRun_SkipsReferencePass_AndSerializesNullReferences()
    {
        using var workspace = await ReferencePassParityTests.OpenParitySolutionAsync();

        var index = await new SymbolIndexer().IndexAsync(workspace, projectFilter: "Core");

        Assert.Null(index.References);
        Assert.NotEmpty(index.Documents);
        Assert.All(index.Documents, d => Assert.All(d.Occurrences, o => Assert.Equal("definition", o.Kind)));
        using var parsed = await WriteAndParseAsync(index);
        Assert.Equal("2.1", parsed.RootElement.GetProperty("metadata").GetProperty("version").GetString());
        Assert.True(parsed.RootElement.TryGetProperty("references", out var references), "references key must be present");
        Assert.Equal(JsonValueKind.Null, references.ValueKind);
    }

    [Fact]
    public async Task FullRun_EmitsReferencesOutsideDocumentOccurrences()
    {
        using var workspace = await ReferencePassParityTests.OpenParitySolutionAsync();

        var index = await new SymbolIndexer().IndexAsync(workspace, projectFilter: null);

        Assert.NotNull(index.References);
        Assert.NotEmpty(index.References!);
        Assert.All(index.Documents, d => Assert.All(d.Occurrences, o => Assert.Equal("definition", o.Kind)));
        var definitionKeys = index.ExternalSymbols.Select(s => s.Symbol).ToHashSet(StringComparer.Ordinal);
        Assert.All(index.References!, r => Assert.Contains(r.Symbol, definitionKeys));
    }

    private static async Task<JsonDocument> WriteAndParseAsync(ScipIndex index)
    {
        var path = Path.Combine(Path.GetTempPath(), $"index-wire-{Guid.NewGuid():N}.json");
        try
        {
            await OutputWriter.WriteAsync(index, path);
            return JsonDocument.Parse(await File.ReadAllTextAsync(path));
        }
        finally
        {
            File.Delete(path);
        }
    }
}
