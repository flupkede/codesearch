using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.MSBuild;

namespace ScipCsharp;

/// <summary>
/// Walks Roslyn compilation symbols and produces a ScipIndex with only definition
/// occurrences (no references). References are resolved lazily at find_impact time
/// by the `find-refs` subcommand, giving 10–50× faster rebuild on large solutions.
/// </summary>
public sealed class SymbolIndexer
{
    public async Task<ScipIndex> IndexAsync(MSBuildWorkspace workspace, string? projectFilter)
    {
        var index = new ScipIndex();
        var solution = workspace.CurrentSolution;

        var projects = solution.Projects;
        if (!string.IsNullOrEmpty(projectFilter))
        {
            var filterName = Path.GetFileNameWithoutExtension(projectFilter);
            projects = projects.Where(p =>
                string.Equals(p.Name, filterName, StringComparison.OrdinalIgnoreCase) ||
                string.Equals(Path.GetFileName(p.FilePath), projectFilter, StringComparison.OrdinalIgnoreCase)).ToList();

            if (!projects.Any())
            {
                var loadedNames = string.Join(", ", solution.Projects.Select(p => p.Name));
                Console.Error.WriteLine(
                    $"[WARN] --filter-project '{projectFilter}' matched zero loaded projects. " +
                    $"Loaded projects: [{loadedNames}]. " +
                    $"The target project likely failed to load (check workspace errors above).");
            }
        }

        // Collect all symbols across all projects
        var symbolMap = new Dictionary<ISymbol, string>(SymbolEqualityComparer.Default);
        // Always compute root from ALL solution projects (not the filtered subset) so that
        // relative paths are consistent between full and incremental rebuilds.
        // Using only the filtered project would produce shorter paths (e.g. "Caches/ICache.cs")
        // vs the full-rebuild paths ("src/ExampleProject.Dam/Caches/ICache.cs"),
        // causing duplicate definitions in find_impact results.
        var projectRoot = FindCommonRoot(solution.Projects.Select(p => p.FilePath).Where(p => p != null).Cast<string>());

        // Materialize project list once so we can log progress (i / total).
        var projectList = projects as IReadOnlyList<Project> ?? projects.ToList();
        var totalProjects = projectList.Count;
        Console.Error.WriteLine($"Compiling {totalProjects} project(s)...");

        var compileSw = System.Diagnostics.Stopwatch.StartNew();
        for (int i = 0; i < totalProjects; i++)
        {
            var project = projectList[i];
            Console.Error.WriteLine($"  [{i + 1}/{totalProjects}] Compiling: {project.Name}");
            var compilation = await project.GetCompilationAsync().ConfigureAwait(false);
            if (compilation is null)
            {
                Console.Error.WriteLine($"[WARN] Could not compile project: {project.Name}");
                continue;
            }

            // Report diagnostics but don't abort
            var diagnostics = compilation.GetDiagnostics()
                .Where(d => d.Severity == DiagnosticSeverity.Error);
            foreach (var diag in diagnostics)
            {
                Console.Error.WriteLine($"[WARN] Compilation error in {project.Name}: {diag}");
            }

            CollectSymbols(compilation.GlobalNamespace, symbolMap);
        }
        compileSw.Stop();
        Console.Error.WriteLine($"Compiled {totalProjects} project(s) in {compileSw.Elapsed.TotalSeconds:F1}s");
        Console.Error.WriteLine($"Collected {symbolMap.Count} project-internal symbols — building definition index...");

        // Walk symbols and emit definition occurrences only.
        // References are intentionally omitted here; they are resolved lazily
        // on first `find_impact` call via `scip-csharp find-refs` and then
        // cached in LMDB so subsequent calls are instant.
        var occurrenceMap = new Dictionary<string, List<ScipOccurrence>>();

        foreach (var (symbol, scipName) in symbolMap)
        {
            foreach (var loc in symbol.Locations)
            {
                if (loc.IsInSource)
                {
                    var relPath = MakeRelative(loc.SourceTree?.FilePath, projectRoot);
                    if (relPath is null) continue;

                    var occ = new ScipOccurrence
                    {
                        Range = LocationToRange(loc),
                        Symbol = scipName,
                        SymbolRoles = 1, // definition bit
                        Kind = "definition",
                    };

                    if (!occurrenceMap.TryGetValue(relPath, out var list))
                    {
                        list = [];
                        occurrenceMap[relPath] = list;
                    }
                    list.Add(occ);
                }
            }
        }

        Console.Error.WriteLine($"Definition index built: {occurrenceMap.Count} file(s)");

        // Build documents
        foreach (var (relPath, occurrences) in occurrenceMap)
        {
            index.Documents.Add(new ScipDocument
            {
                RelativePath = relPath,
                Occurrences = occurrences,
            });
        }

        // Build external symbols list (used by Rust side to populate simple-name index)
        foreach (var (_, scipName) in symbolMap)
        {
            index.ExternalSymbols.Add(new ScipSymbolInfo
            {
                Symbol = scipName,
            });
        }

        return index;
    }

    internal static void CollectSymbols(INamespaceSymbol ns, Dictionary<ISymbol, string> map)
    {
        foreach (var child in ns.GetMembers())
        {
            if (child is INamespaceSymbol childNs)
            {
                CollectSymbols(childNs, map);
            }
            else if (child is INamedTypeSymbol type)
            {
                CollectTypeSymbols(type, map);
            }
        }
    }

    internal static void CollectTypeSymbols(INamedTypeSymbol type, Dictionary<ISymbol, string> map)
    {
        // Skip compiler-generated types (anonymous types, display classes, etc.)
        if (type.IsImplicitlyDeclared || type.Name.Contains('<') || type.Name.StartsWith("<"))
            return;

        // Skip types from referenced assemblies (System.*, Microsoft.*, NuGet packages).
        // Project-internal types always have at least one IsInSource location (the .cs file
        // where they are declared). External types live only in compiled DLLs — they have
        // no source locations at all. Filtering here eliminates thousands of framework
        // symbols before they even reach FindReferencesAsync, giving a 10-100× speedup
        // on large enterprise solutions.
        if (!type.Locations.Any(l => l.IsInSource))
            return;

        var scipName = SymbolToScipName(type);
        if (!string.IsNullOrEmpty(scipName))
            map[type] = scipName;

        // Members
        foreach (var member in type.GetMembers())
        {
            if (member.IsImplicitlyDeclared)
                continue;

            if (member is IMethodSymbol method)
            {
                // Skip property getters/setters, constructors (if parameterless), operators, and delegates
                if (method.AssociatedSymbol is IPropertySymbol)
                    continue;
                if (method.MethodKind is MethodKind.Constructor or MethodKind.StaticConstructor)
                    continue;
                if (method.MethodKind is MethodKind.Conversion or MethodKind.UserDefinedOperator or MethodKind.BuiltinOperator)
                    continue;

                var memberScip = SymbolToScipName(method);
                if (!string.IsNullOrEmpty(memberScip))
                    map[method] = memberScip;
            }
            else if (member is IPropertySymbol prop)
            {
                var memberScip = SymbolToScipName(prop);
                if (!string.IsNullOrEmpty(memberScip))
                    map[prop] = memberScip;
            }
            else if (member is IFieldSymbol field)
            {
                // Skip backing fields for properties
                if (field.AssociatedSymbol is IPropertySymbol)
                    continue;
                // Skip enum members (they show up as fields)
                if (field.ContainingType.TypeKind == TypeKind.Enum)
                    continue;

                var memberScip = SymbolToScipName(field);
                if (!string.IsNullOrEmpty(memberScip))
                    map[field] = memberScip;
            }
            else if (member is IEventSymbol evt)
            {
                var memberScip = SymbolToScipName(evt);
                if (!string.IsNullOrEmpty(memberScip))
                    map[evt] = memberScip;
            }
            else if (member is INamedTypeSymbol nestedType)
            {
                CollectTypeSymbols(nestedType, map);
            }
        }
    }

    /// <summary>
    /// Converts a Roslyn symbol to a SCIP-style symbol name.
    /// Format: csharp &lt;namespace&gt; . &lt;Type&gt;#&lt;member&gt;(&lt;params&gt;).
    /// </summary>
    internal static string SymbolToScipName(ISymbol symbol)
    {
        if (symbol is INamedTypeSymbol type)
        {
            var ns = type.ContainingNamespace?.ToDisplayString(SymbolDisplayFormat.FullyQualifiedFormat);
            if (ns?.StartsWith("global::") == true)
                ns = ns["global::".Length..];
            if (string.IsNullOrEmpty(ns))
                return $"csharp . . {type.Name}#";
            return $"csharp {ns} . {type.Name}#";
        }

        var containingType = symbol.ContainingType;
        if (containingType is null)
            return "";

        var typeNs = containingType.ContainingNamespace?.ToDisplayString(SymbolDisplayFormat.FullyQualifiedFormat);
        if (typeNs?.StartsWith("global::") == true)
            typeNs = typeNs["global::".Length..];

        var typeName = containingType.Name;
        var prefix = string.IsNullOrEmpty(typeNs)
            ? $"csharp . . {typeName}#"
            : $"csharp {typeNs} . {typeName}#";

        return symbol switch
        {
            IMethodSymbol method => $"{prefix}{method.Name}({FormatParameters(method.Parameters)}).",
            IPropertySymbol prop => $"{prefix}{prop.Name}",
            IFieldSymbol field => $"{prefix}{field.Name}",
            IEventSymbol evt => $"{prefix}{evt.Name}",
            _ => "",
        };
    }

    internal static string FormatParameters(IEnumerable<IParameterSymbol> parameters)
    {
        return string.Join(", ", parameters.Select(p =>
        {
            var type = p.Type.ToDisplayString(SymbolDisplayFormat.MinimallyQualifiedFormat);
            return p.RefKind switch
            {
                RefKind.Ref => $"ref {type}",
                RefKind.Out => $"out {type}",
                RefKind.In => $"in {type}",
                _ => type,
            };
        }));
    }

    internal static List<int> LocationToRange(Location loc)
    {
        var lineSpan = loc.GetLineSpan();
        return
        [
            lineSpan.StartLinePosition.Line + 1,  // 1-based line
            lineSpan.StartLinePosition.Character + 1,  // 1-based column
            lineSpan.EndLinePosition.Line + 1,
            lineSpan.EndLinePosition.Character + 1,
        ];
    }

    internal static string? MakeRelative(string? filePath, string? root)
    {
        if (filePath is null || root is null)
            return filePath?.Replace('\\', '/');

        if (filePath.StartsWith(root, StringComparison.OrdinalIgnoreCase))
        {
            var rel = filePath[root.Length..].TrimStart('\\', '/');
            return rel.Replace('\\', '/');
        }

        return filePath.Replace('\\', '/');
    }

    internal static string? FindCommonRoot(IEnumerable<string> paths)
    {
        var list = paths.ToList();
        if (list.Count == 0)
            return null;

        var root = list[0];
        foreach (var p in list)
        {
            var common = CommonPrefix(root, p);
            if (common.Length < root.Length)
                root = common;
        }

        // Trim to last directory separator
        var lastSep = root.LastIndexOfAny(['\\', '/']);
        return lastSep > 0 ? root[..lastSep] : root;
    }

    internal static string CommonPrefix(string a, string b)
    {
        var len = Math.Min(a.Length, b.Length);
        for (int i = 0; i < len; i++)
        {
            if (char.ToLower(a[i]) != char.ToLower(b[i]))
                return a[..i];
        }
        return a[..len];
    }
}
