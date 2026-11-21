namespace ScipCsharp;

/// <summary>
/// Strip-retry decision logic for analyzer-only PackageReferences a restore
/// cannot resolve (NU1101/NU1102). Analyzers feed the compiler's diagnostic
/// pipeline but never the semantic model, so an index built without them is
/// complete (VS2026 loads the same solution) — leaving them in fails the
/// restore for every project and degrades the C# index. All members are pure
/// and side-effect free so tests can drive every branch without a restore.
/// Compiled into the test project via a linked Compile item (same pattern as
/// WorkspaceLoad.cs).
/// </summary>
internal static partial class AnalyzerStrip
{
    // NU1101: "Unable to find package <id>. No packages exist with this id
    // in source(s): ...". NU1102: "Unable to find package <id> with version
    // (>= x.y)". Quoted ids occur in some host wordings; a bare id ends at
    // whitespace or a trailing period.
    [System.Text.RegularExpressions.GeneratedRegex(
        @"NU110[12][^\r\n]*?Unable to find package (?:'(?<id>[^']+)'|(?<id>[A-Za-z0-9][A-Za-z0-9_.+-]*))",
        System.Text.RegularExpressions.RegexOptions.IgnoreCase
        | System.Text.RegularExpressions.RegexOptions.CultureInvariant)]
    private static partial System.Text.RegularExpressions.Regex UnresolvedPackageRegex();

    /// <summary>
    /// Package ids the restore output reports unresolved (NU1101/NU1102),
    /// first occurrence first, deduplicated. Empty for other failure classes
    /// — SDK gaps, feed auth (NU1301) and version conflicts must never match.
    /// </summary>
    internal static List<string> ExtractUnresolvedPackages(string restoreOutput)
    {
        if (string.IsNullOrWhiteSpace(restoreOutput))
        {
            return [];
        }
        var ids = new List<string>();
        foreach (var match in UnresolvedPackageRegex().Matches(restoreOutput).Cast<System.Text.RegularExpressions.Match>())
        {
            var id = match.Groups["id"].Value.TrimEnd('.');
            if (id.Length > 0 && !ids.Contains(id))
            {
                ids.Add(id);
            }
        }
        return ids;
    }

    /// <summary>
    /// True when a PackageReference's metadata text (its attributes plus the
    /// child-element block — never the package id, which may legitimately
    /// contain "Analyzers") is provably compiler-consumed only: the
    /// analyzer-shaped metadata block (<c>PrivateAssets</c> /
    /// <c>IncludeAssets</c> mentioning analyzers, or <c>PrivateAssets=all</c>).
    /// A bare CPM-era reference carries no metadata in the csproj and is NOT
    /// strippable — stripping an unknown compile-time dependency would trade
    /// a degraded index for CS0246 noise.
    /// </summary>
    internal static bool ElementLooksAnalyzerOnly(string elementText) =>
        elementText.Contains("analyzers", StringComparison.OrdinalIgnoreCase)
        || System.Text.RegularExpressions.Regex.IsMatch(
            elementText, @"PrivateAssets\s*=\s*""all""", System.Text.RegularExpressions.RegexOptions.IgnoreCase);

    // Self-closing and child-element forms; attrs+body carry everything the
    // classifier may look at (attributes and the metadata block) while the
    // Include value is captured separately so a package id like
    // "Vendor.Analyzers" can never read as analyzer-shaped metadata.
    [System.Text.RegularExpressions.GeneratedRegex(
        @"<PackageReference\b[^>]*Include\s*=\s*""(?<id>[^""]+)""(?<attrs>[^>]*?)(?:/>|>(?<body>(?:(?!</PackageReference>).)*)</PackageReference>)",
        System.Text.RegularExpressions.RegexOptions.IgnoreCase
        | System.Text.RegularExpressions.RegexOptions.CultureInvariant
        | System.Text.RegularExpressions.RegexOptions.Singleline)]
    private static partial System.Text.RegularExpressions.Regex PackageReferenceRegex();

    /// <summary>
    /// The subset of <paramref name="packageIds"/> whose references across
    /// the solution's csprojs are ALL analyzer-shaped. A package referenced
    /// by no csproj, or with at least one non-analyzer reference, is kept —
    /// when in doubt, don't strip. The walk is bounded (300 files, 256 KB
    /// each) so a huge tree cannot stall the restore path.
    /// </summary>
    internal static List<string> SelectStrippable(List<string> packageIds, string solutionDir)
    {
        if (packageIds.Count == 0)
        {
            return [];
        }
        List<string> csprojs;
        try
        {
            csprojs = Directory
                .EnumerateFiles(solutionDir, "*.csproj", SearchOption.AllDirectories)
                .Take(300)
                .ToList();
        }
        catch (Exception)
        {
            // Unreadable tree — keep today's behavior (no strip retry).
            return [];
        }
        var strippable = new List<string>();
        foreach (var id in packageIds)
        {
            var references = new List<string>();
            foreach (var csproj in csprojs)
            {
                var text = ReadHead(csproj);
                if (text is null)
                {
                    continue;
                }
                foreach (var match in PackageReferenceRegex().Matches(text).Cast<System.Text.RegularExpressions.Match>())
                {
                    if (string.Equals(match.Groups["id"].Value, id, StringComparison.OrdinalIgnoreCase))
                    {
                        references.Add(
                            match.Groups["attrs"].Value + " " + match.Groups["body"].Value);
                    }
                }
            }
            if (references.Count > 0 && references.All(ElementLooksAnalyzerOnly))
            {
                strippable.Add(id);
            }
        }
        return strippable;
    }

    private static string? ReadHead(string path)
    {
        try
        {
            using var reader = new StreamReader(path);
            var buffer = new char[256 * 1024];
            var read = reader.Read(buffer, 0, buffer.Length);
            return new string(buffer, 0, read);
        }
        catch (Exception)
        {
            // Locked or vanished mid-scan: this file contributes nothing.
            return null;
        }
    }

    /// <summary>
    /// The generated targets file: removes the stripped PackageReferences at
    /// evaluation time, so both the restore retry and the subsequent
    /// design-time load see a consistent project (the restored assets do not
    /// contain the package either). Ids are XML-escaped; never stale — the
    /// file is written fresh per retry.
    /// </summary>
    internal static string TargetsFileContent(List<string> packageIds)
    {
        var sb = new System.Text.StringBuilder();
        sb.AppendLine("<Project>");
        sb.AppendLine("  <!-- Generated by scip-csharp: analyzer-only PackageReferences the");
        sb.AppendLine("       configured feeds cannot resolve (NU1101/NU1102). Analyzers do not");
        sb.AppendLine("       affect the semantic model; indexing proceeds without them. -->");
        sb.AppendLine("  <ItemGroup>");
        foreach (var id in packageIds)
        {
            sb.AppendLine($"    <PackageReference Remove=\"{Escape(id)}\" />");
        }
        sb.AppendLine("  </ItemGroup>");
        sb.AppendLine("</Project>");
        return sb.ToString();
    }

    private static string Escape(string id) => id
        .Replace("&", "&amp;")
        .Replace("<", "&lt;")
        .Replace(">", "&gt;")
        .Replace("\"", "&quot;");
}
