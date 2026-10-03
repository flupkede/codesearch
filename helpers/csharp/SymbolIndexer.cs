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
        // (error count, summary line) per erroring project — emitted after
        // the loop, loudest first, so the warnings-channel cap can never
        // crowd out the worst project: that is the one the user needs named.
        var erroringProjects = new List<(int Count, string Line)>();
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

            // Report diagnostics but don't abort. A single broken reference
            // resolution cascades into tens of thousands of errors per
            // project (CS0518 "System.Object not defined" turns every type
            // use into CS0246) — that is ONE root cause, not N problems, so
            // the warnings channel carries ONE aggregated line per project
            // (count + dominant code + first specimen). Symbols below are
            // still collected: a Roslyn compilation with errors yields a
            // complete symbol tree — the same reason IntelliSense works
            // while the build is red.
            var diagnostics = compilation.GetDiagnostics()
                .Where(d => d.Severity == DiagnosticSeverity.Error)
                .ToList();
            if (diagnostics.Count > 0)
            {
                erroringProjects.Add((diagnostics.Count,
                    WarningLineFor(project.Name, diagnostics)));
            }

            CollectSymbols(compilation.GlobalNamespace, symbolMap);
        }
        compileSw.Stop();
        foreach (var (_, line) in erroringProjects.OrderByDescending(e => e.Count))
        {
            Console.Error.WriteLine(line);
        }
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

    /// One-line aggregation of a project's compilation errors for the
    /// index-level warnings channel: count, dominant error code, first
    /// specimen (capped — the full diagnostic can run very long). Wrapped
    /// with the [WARN] marker the Rust capture keys on by
    /// <see cref="WarningLineFor"/>. One broken reference resolution
    /// cascades into tens of thousands of diagnostics; the channel must
    /// carry the root cause, not the cascade.
    /// </summary>
    internal static string CompilationErrorSummaryLine(
        string project, IReadOnlyList<Diagnostic> errors)
    {
        var byCode = errors.GroupBy(d => d.Id)
            .OrderByDescending(g => g.Count()).ToList();
        var first = errors[0].ToString();
        if (first.Length > 160) first = first[..160] + "…";
        return
            $"Compilation errors in {project}: {errors.Count} error(s) — dominant " +
            $"{byCode[0].Key} x{byCode[0].Count()}; first: {first}";
    }

    /// The channel-emittable form. The `[WARN] ` prefix is LOAD-BEARING:
    /// the Rust capture (`is_helper_warning_line`) keys on it, so a dropped
    /// prefix does not flood the channel — it silently empties it.
    internal static string WarningLineFor(string project, IReadOnlyList<Diagnostic> errors) =>
        "[WARN] " + CompilationErrorSummaryLine(project, errors);

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
    /// Format: csharp &lt;namespace&gt; . &lt;ContainingTypePath&gt;#&lt;member&gt;(&lt;params&gt;).
    /// Distinctness guarantees (see KeyFormatTests): generic arity is kept
    /// (Foo`1 vs Foo, M`1 vs M), nested types carry their full containing-type
    /// chain (Outer1.Inner vs Outer2.Inner) and parameter types are fully
    /// qualified (A.P vs B.P). Any change here must bump the index version
    /// (ScipModels "2.0") and SCIP_KEY_FORMAT so old indexes rebuild.
    /// </summary>
    internal static string SymbolToScipName(ISymbol symbol)
    {
        if (symbol is INamedTypeSymbol type)
        {
            var ns = type.ContainingNamespace?.ToDisplayString(SymbolDisplayFormat.FullyQualifiedFormat);
            if (ns?.StartsWith("global::") == true)
                ns = ns["global::".Length..];
            var typePath = ContainingTypePath(type);
            if (string.IsNullOrEmpty(ns))
                return $"csharp . . {typePath}#";
            return $"csharp {ns} . {typePath}#";
        }

        var containingType = symbol.ContainingType;
        if (containingType is null)
            return "";

        var typeNs = containingType.ContainingNamespace?.ToDisplayString(SymbolDisplayFormat.FullyQualifiedFormat);
        if (typeNs?.StartsWith("global::") == true)
            typeNs = typeNs["global::".Length..];

        var containingPath = ContainingTypePath(containingType);
        var prefix = string.IsNullOrEmpty(typeNs)
            ? $"csharp . . {containingPath}#"
            : $"csharp {typeNs} . {containingPath}#";

        return symbol switch
        {
            // Arity on the method name (M`1) keeps void M() and void M<T>()
            // on distinct keys.
            IMethodSymbol method => method.Arity > 0
                ? $"{prefix}{method.Name}`{method.Arity}({FormatParameters(method.Parameters)})."
                : $"{prefix}{method.Name}({FormatParameters(method.Parameters)}).",
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
            // Fully qualified: MinimallyQualifiedFormat displayed both A.Foo
            // and B.Foo as `Foo`, collapsing distinct overloads onto one key.
            var type = p.Type.ToDisplayString(SymbolDisplayFormat.FullyQualifiedFormat);
            if (type.StartsWith("global::", StringComparison.Ordinal))
                type = type["global::".Length..];
            return p.RefKind switch
            {
                RefKind.Ref => $"ref {type}",
                RefKind.Out => $"out {type}",
                RefKind.In => $"in {type}",
                _ => type,
            };
        }));
    }

    /// <summary>
    /// Type name with generic arity (Roslyn/ECMA backtick convention):
    /// `Foo`, `Foo`1`. Without the arity, `class Foo&lt;T&gt;` and `class Foo`
    /// collapse onto one key.
    /// </summary>
    internal static string FormatTypeRef(INamedTypeSymbol t) =>
        t.Arity > 0 ? $"{t.Name}`{t.Arity}" : t.Name;

    /// <summary>
    /// Containing-type chain outermost-first including <paramref name="t"/>
    /// itself, e.g. `Outer`1.Inner`. Only the immediate type name would let
    /// `Ns.Outer1.Inner` and `Ns.Outer2.Inner` collide.
    /// </summary>
    internal static string ContainingTypePath(INamedTypeSymbol t)
    {
        var segments = new List<string>();
        for (var current = (INamedTypeSymbol?)t; current is not null; current = current.ContainingType)
            segments.Add(FormatTypeRef(current));
        segments.Reverse();
        return string.Join(".", segments);
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
