namespace ScipCsharp;

/// <summary>
/// Output model for the symbol index. Serialized as JSON.
/// </summary>
public sealed class ScipIndex
{
    public ScipMetadata Metadata { get; init; } = new();
    public List<ScipDocument> Documents { get; init; } = [];
    public List<ScipSymbolInfo> ExternalSymbols { get; init; } = [];
}

public sealed class ScipMetadata
{
    public string Version { get; init; } = "1.0";
    public string ToolInfo { get; init; } = "scip-csharp";
}

public sealed class ScipDocument
{
    public string RelativePath { get; init; } = "";
    public List<ScipOccurrence> Occurrences { get; init; } = [];
}

public sealed class ScipOccurrence
{
    /// <summary>[start_line, start_col, end_line, end_col]</summary>
    public List<int> Range { get; init; } = [];
    public string Symbol { get; init; } = "";
    public int SymbolRoles { get; init; }
    public string Kind { get; init; } = ""; // "definition" or "reference"
}

public sealed class ScipSymbolInfo
{
    public string Symbol { get; init; } = "";
    public List<string> Documentation { get; init; } = [];
}
