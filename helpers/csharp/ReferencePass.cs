using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CSharp;
using Microsoft.CodeAnalysis.CSharp.Syntax;

namespace ScipCsharp;

/// <summary>
/// Single-pass reference collection for `index`: every in-source document is
/// bound once and each reference site is keyed with
/// <see cref="SymbolIndexer.SymbolToScipName"/>. Attribution mirrors the
/// default (bidirectional) cascade of SymbolFinder.FindReferencesAsync that
/// the lazy `find-refs` path uses: a site counts for every definition key in
/// its inheritance component (overrides and interface implementations, also
/// through metadata members), and constructors and accessors fold into their
/// type, property or event.
/// </summary>
internal sealed class ReferencePass
{
    public const string ParallelismEnvName = "CODESEARCH_SCIP_REFS_PARALLELISM";
    private const int MaxDefaultParallelism = 8;

    private readonly string? _projectRoot;
    private readonly HashSet<string> _sourceAssemblies;
    private readonly object _gate = new();
    private readonly Dictionary<string, int> _keyIds = new(StringComparer.Ordinal);
    private readonly List<string> _keys = [];
    private readonly Dictionary<string, int> _fileIds = new(StringComparer.Ordinal);
    private readonly List<string> _files = [];
    private readonly List<Site> _sites = [];

    private readonly record struct Site(int Key, int File, int StartLine, int StartColumn, int EndLine);

    private readonly record struct RawSite(string Key, string MappedPath, int StartLine, int StartColumn, int EndLine);

    internal ReferencePass(string? projectRoot, IEnumerable<string?> sourceAssemblyNames)
    {
        _projectRoot = projectRoot;
        _sourceAssemblies = new HashSet<string>(
            sourceAssemblyNames.OfType<string>(), StringComparer.OrdinalIgnoreCase);
    }

    /// <summary>Distinct files carrying at least one record of the last <see cref="Build"/>.</summary>
    internal int FileCount { get; private set; }

    /// <summary>
    /// Documents bound concurrently: <c>CODESEARCH_SCIP_REFS_PARALLELISM</c>
    /// when set to a positive integer, otherwise the processor count capped
    /// at 8. Unparseable values fall back to the default.
    /// </summary>
    internal static int Parallelism()
    {
        var raw = Environment.GetEnvironmentVariable(ParallelismEnvName);
        if (int.TryParse(raw, out var degree) && degree > 0)
        {
            return degree;
        }
        return Math.Min(Environment.ProcessorCount, MaxDefaultParallelism);
    }

    /// <summary>
    /// Binds every syntax tree of <paramref name="compilation"/> that the
    /// definition pass would also emit (in source, with a relative path).
    /// One failing document is logged and skipped, never fatal.
    /// </summary>
    internal async Task CollectAsync(Compilation compilation, CancellationToken cancellationToken = default)
    {
        var trees = compilation.SyntaxTrees
            .Select(tree => (Tree: tree, File: SymbolIndexer.MakeRelative(tree.FilePath, _projectRoot)))
            .Where(item => item.File is not null)
            .ToList();
        var options = new ParallelOptions
        {
            MaxDegreeOfParallelism = Parallelism(),
            CancellationToken = cancellationToken,
        };
        await Parallel.ForEachAsync(trees, options, async (item, ct) =>
        {
            try
            {
                var sites = await CollectDocumentAsync(compilation, item.Tree, ct).ConfigureAwait(false);
                Merge(item.File!, sites);
            }
            catch (Exception ex) when (ex is not OperationCanceledException)
            {
                Console.Error.WriteLine(
                    $"[WARN] reference pass failed for {item.File}: {ex.GetType().Name}: {ex.Message}");
            }
        }).ConfigureAwait(false);
    }

    private async Task<List<RawSite>> CollectDocumentAsync(
        Compilation compilation, SyntaxTree tree, CancellationToken ct)
    {
        var root = await tree.GetRootAsync(ct).ConfigureAwait(false);
        var document = new DocumentSites(this, compilation.GetSemanticModel(tree), ct);
        // Doc-comment crefs live in structured trivia; FindReferencesAsync reports them.
        foreach (var node in root.DescendantNodes(descendIntoTrivia: true))
        {
            document.Visit(node);
        }
        return document.Sites;
    }

    private void Merge(string fallbackFile, List<RawSite> sites)
    {
        if (sites.Count == 0)
            return;
        lock (_gate)
        {
            // Group by the MAPPED path: a Razor-generated tree's sites map to
            // their .razor/.cshtml source file, not the tree's virtual path.
            foreach (var group in sites.GroupBy(
                         s => string.IsNullOrEmpty(s.MappedPath) ? fallbackFile : s.MappedPath))
            {
                var file = SymbolIndexer.MakeRelative(group.Key, _projectRoot) ?? group.Key;
                var fileId = Intern(_fileIds, _files, file);
                foreach (var site in group)
                {
                    _sites.Add(new Site(
                        Intern(_keyIds, _keys, site.Key), fileId, site.StartLine, site.StartColumn, site.EndLine));
                }
            }
        }
    }

    private static int Intern(Dictionary<string, int> ids, List<string> values, string value)
    {
        if (!ids.TryGetValue(value, out var id))
        {
            id = values.Count;
            values.Add(value);
            ids[value] = id;
        }
        return id;
    }

    /// <summary>
    /// A site is kept when its symbol is declared in a solution assembly or
    /// can bridge an inheritance component (an overridable or interface
    /// member, e.g. object.ToString linking every override). Everything else
    /// — the bulk of framework calls — can never reach a definition key.
    /// </summary>
    private bool IsRelevant(ISymbol symbol) =>
        (symbol.ContainingAssembly?.Name is { } assembly && _sourceAssemblies.Contains(assembly))
        || (symbol is IMethodSymbol or IPropertySymbol or IEventSymbol
            && (symbol.IsVirtual || symbol.IsAbstract || symbol.IsOverride
                || symbol.ContainingType?.TypeKind == TypeKind.Interface));

    private string? KeyFor(ISymbol symbol)
    {
        var normalized = Normalize(symbol);
        if (normalized is null || !IsRelevant(normalized))
            return null;
        var key = SymbolIndexer.SymbolToScipName(normalized);
        return key.Length == 0 ? null : key;
    }

    /// <summary>
    /// Attributes every collected site to the definition keys of its
    /// inheritance component and returns the deduplicated records, ordered
    /// by symbol, file and position. Releases the collected sites.
    /// </summary>
    internal List<ScipReferenceRecord> Build(IReadOnlyDictionary<ISymbol, string> definitions)
    {
        var edges = new List<(int From, int To)>();
        var definitionIds = new HashSet<int>();
        foreach (var (symbol, key) in definitions)
        {
            definitionIds.Add(Intern(_keyIds, _keys, key));
            AddHierarchyEdges(symbol, edges);
        }

        var components = new UnionFind(_keys.Count);
        foreach (var (from, to) in edges)
        {
            components.Union(from, to);
        }
        var definitionsByComponent = new Dictionary<int, List<int>>();
        foreach (var id in definitionIds)
        {
            var root = components.Find(id);
            if (!definitionsByComponent.TryGetValue(root, out var members))
            {
                members = [];
                definitionsByComponent[root] = members;
            }
            members.Add(id);
        }

        var attributed = new HashSet<Site>();
        foreach (var site in _sites)
        {
            if (!definitionsByComponent.TryGetValue(components.Find(site.Key), out var targets))
                continue;
            foreach (var target in targets)
            {
                attributed.Add(site with { Key = target });
            }
        }
        _sites.Clear();
        _sites.TrimExcess();

        FileCount = attributed.Select(site => site.File).Distinct().Count();
        return attributed
            .OrderBy(site => _keys[site.Key], StringComparer.Ordinal)
            .ThenBy(site => _files[site.File], StringComparer.Ordinal)
            .ThenBy(site => site.StartLine)
            .ThenBy(site => site.StartColumn)
            .Select(site => new ScipReferenceRecord
            {
                Symbol = _keys[site.Key],
                File = _files[site.File],
                StartLine = site.StartLine,
                EndLine = site.EndLine,
            })
            .ToList();
    }

    private void AddHierarchyEdges(ISymbol definition, List<(int From, int To)> edges)
    {
        switch (definition)
        {
            case INamedTypeSymbol type:
                foreach (var contract in type.AllInterfaces)
                {
                    foreach (var member in contract.GetMembers())
                    {
                        if (member is not (IMethodSymbol { MethodKind: MethodKind.Ordinary } or IPropertySymbol or IEventSymbol))
                            continue;
                        if (type.FindImplementationForInterfaceMember(member) is { } implementation)
                            Link(implementation, member, edges);
                    }
                }
                return;
            case IMethodSymbol or IPropertySymbol or IEventSymbol:
                var current = definition;
                for (var overridden = Overridden(current); overridden is not null; overridden = Overridden(current))
                {
                    Link(current, overridden, edges);
                    current = overridden;
                }
                foreach (var implemented in ExplicitImplementations(definition))
                {
                    Link(definition, implemented, edges);
                }
                return;
        }
    }

    private void Link(ISymbol from, ISymbol to, List<(int From, int To)> edges)
    {
        if (Normalize(from) is not { } source || Normalize(to) is not { } target)
            return;
        var sourceKey = SymbolIndexer.SymbolToScipName(source);
        var targetKey = SymbolIndexer.SymbolToScipName(target);
        if (sourceKey.Length == 0 || targetKey.Length == 0)
            return;
        edges.Add((Intern(_keyIds, _keys, sourceKey), Intern(_keyIds, _keys, targetKey)));
    }

    private static ISymbol? Overridden(ISymbol symbol) => symbol switch
    {
        IMethodSymbol method => method.OverriddenMethod,
        IPropertySymbol property => property.OverriddenProperty,
        IEventSymbol evt => evt.OverriddenEvent,
        _ => null,
    };

    private static IEnumerable<ISymbol> ExplicitImplementations(ISymbol symbol) => symbol switch
    {
        IMethodSymbol method => method.ExplicitInterfaceImplementations,
        IPropertySymbol property => property.ExplicitInterfaceImplementations,
        IEventSymbol evt => evt.ExplicitInterfaceImplementations,
        _ => [],
    };

    private sealed class UnionFind(int size)
    {
        private readonly int[] _parent = Enumerable.Range(0, size).ToArray();

        internal int Find(int id)
        {
            while (_parent[id] != id)
            {
                _parent[id] = _parent[_parent[id]];
                id = _parent[id];
            }
            return id;
        }

        internal void Union(int a, int b) => _parent[Find(a)] = Find(b);
    }

    /// <summary>
    /// Maps a bound symbol onto the declaration its key is built from:
    /// constructed generics and reduced extensions to their definition,
    /// accessors to their property or event, constructors to their type.
    /// Locals, parameters, lambdas and local functions have no key.
    /// </summary>
    internal static ISymbol? Normalize(ISymbol symbol)
    {
        switch (symbol)
        {
            case IMethodSymbol method:
                method = (method.ReducedFrom ?? method).OriginalDefinition;
                return method.MethodKind switch
                {
                    MethodKind.PropertyGet or MethodKind.PropertySet
                        or MethodKind.EventAdd or MethodKind.EventRemove or MethodKind.EventRaise
                        => method.AssociatedSymbol?.OriginalDefinition,
                    MethodKind.Constructor or MethodKind.StaticConstructor or MethodKind.Destructor
                        => method.ContainingType?.OriginalDefinition,
                    MethodKind.LocalFunction or MethodKind.AnonymousFunction => null,
                    _ => method,
                };
            case INamedTypeSymbol or IPropertySymbol or IFieldSymbol or IEventSymbol:
                return symbol.OriginalDefinition;
            default:
                return null;
        }
    }

    private sealed class DocumentSites(ReferencePass pass, SemanticModel model, CancellationToken ct)
    {
        private readonly Dictionary<ISymbol, string?> _keyCache = new(SymbolEqualityComparer.Default);

        internal List<RawSite> Sites { get; } = [];

        internal void Visit(SyntaxNode node)
        {
            switch (node)
            {
                // `var` binds to the inferred type, but is never a reference to it.
                case IdentifierNameSyntax { IsVar: true }:
                    return;
                // The alias name of `using X = Foo;` is a declaration.
                case IdentifierNameSyntax { Parent: NameEqualsSyntax { Parent: UsingDirectiveSyntax } }:
                    return;
                case SimpleNameSyntax name:
                    Add(model.GetSymbolInfo(name, ct), name.Identifier.GetLocation());
                    return;
                case ImplicitObjectCreationExpressionSyntax implicitNew:
                    Add(model.GetSymbolInfo(implicitNew, ct), implicitNew.NewKeyword.GetLocation());
                    return;
                case ConstructorInitializerSyntax initializer:
                    Add(model.GetSymbolInfo(initializer, ct), initializer.ThisOrBaseKeyword.GetLocation());
                    return;
                case ElementAccessExpressionSyntax access:
                    Add(model.GetSymbolInfo(access, ct), access.ArgumentList.OpenBracketToken.GetLocation());
                    return;
                case ElementBindingExpressionSyntax binding:
                    Add(model.GetSymbolInfo(binding, ct), binding.ArgumentList.OpenBracketToken.GetLocation());
                    return;
                case CommonForEachStatementSyntax forEach:
                    VisitForEach(forEach);
                    return;
                // FindReferencesAsync counts the implicit Dispose of a synchronous
                // `using`, never of `await using`.
                case UsingStatementSyntax usingStatement when usingStatement.AwaitKeyword.IsKind(SyntaxKind.None):
                    if (usingStatement.Declaration is { } usingDeclaration)
                        AddImplicitDispose(usingDeclaration, usingStatement.UsingKeyword);
                    else
                        AddImplicitDispose(usingStatement.Expression, usingStatement.UsingKeyword);
                    return;
                case LocalDeclarationStatementSyntax local
                    when local.UsingKeyword.IsKind(SyntaxKind.UsingKeyword) && local.AwaitKeyword.IsKind(SyntaxKind.None):
                    AddImplicitDispose(local.Declaration, local.UsingKeyword);
                    return;
                case AssignmentExpressionSyntax { Left: TupleExpressionSyntax or DeclarationExpressionSyntax } assignment
                    when assignment.IsKind(SyntaxKind.SimpleAssignmentExpression):
                    AddDeconstruction(model.GetDeconstructionInfo(assignment), assignment.GetFirstToken().GetLocation());
                    return;
                case AwaitExpressionSyntax awaitExpression:
                    var awaitInfo = model.GetAwaitExpressionInfo(awaitExpression);
                    var awaitLocation = awaitExpression.AwaitKeyword.GetLocation();
                    Add(awaitInfo.GetAwaiterMethod, awaitLocation);
                    Add(awaitInfo.IsCompletedProperty, awaitLocation);
                    Add(awaitInfo.GetResultMethod, awaitLocation);
                    return;
                case InitializerExpressionSyntax initializer
                    when initializer.IsKind(SyntaxKind.CollectionInitializerExpression):
                    foreach (var element in initializer.Expressions)
                    {
                        Add(model.GetCollectionInitializerSymbolInfo(element, ct), element.GetFirstToken().GetLocation());
                    }
                    return;
            }
        }

        private void VisitForEach(CommonForEachStatementSyntax forEach)
        {
            var info = model.GetForEachStatementInfo(forEach);
            var location = forEach.ForEachKeyword.GetLocation();
            Add(info.GetEnumeratorMethod, location);
            Add(info.MoveNextMethod, location);
            Add(info.CurrentProperty, location);
            // DisposeMethod is left out: FindReferencesAsync does not count it.
            if (forEach is ForEachVariableStatementSyntax variables)
            {
                AddDeconstruction(model.GetDeconstructionInfo(variables), location);
            }
        }

        private void AddImplicitDispose(VariableDeclarationSyntax declaration, SyntaxToken usingKeyword)
        {
            foreach (var variable in declaration.Variables)
            {
                AddImplicitDispose(variable.Initializer?.Value, usingKeyword);
            }
        }

        // Attributed like FindReferencesAsync: to the resource's own Dispose
        // implementation, typed by the initializer rather than the declaration.
        private void AddImplicitDispose(ExpressionSyntax? resource, SyntaxToken usingKeyword)
        {
            if (resource is null || model.GetTypeInfo(resource, ct).Type is not { } type)
                return;
            var disposable = model.Compilation.GetSpecialType(SpecialType.System_IDisposable);
            var dispose = disposable.GetMembers(nameof(IDisposable.Dispose)).FirstOrDefault();
            if (dispose is null)
                return;
            var implementation = type.FindImplementationForInterfaceMember(dispose);
            if (implementation is null
                && (SymbolEqualityComparer.Default.Equals(type, disposable)
                    || type.AllInterfaces.Contains(disposable, SymbolEqualityComparer.Default)))
            {
                implementation = dispose;
            }
            Add(implementation, usingKeyword.GetLocation());
        }

        private void AddDeconstruction(DeconstructionInfo info, Location location)
        {
            Add(info.Method, location);
            foreach (var nested in info.Nested)
            {
                AddDeconstruction(nested, location);
            }
        }

        // Candidates count like FindReferencesAsync counts them: every one,
        // so a nameof(...) method group or a failed overload resolution is
        // attributed to each overload it could mean.
        private void Add(SymbolInfo info, Location location)
        {
            if (info.Symbol is { } symbol)
            {
                Add(symbol, location);
                return;
            }
            foreach (var candidate in info.CandidateSymbols)
            {
                Add(candidate, location);
            }
        }

        private void Add(ISymbol? symbol, Location location)
        {
            if (symbol is null)
                return;
            if (!_keyCache.TryGetValue(symbol, out var key))
            {
                key = pass.KeyFor(symbol);
                _keyCache[symbol] = key;
            }
            if (key is null)
                return;
            // Mapped span: #line directives (Razor source generators) resolve
            // to the .razor/.cshtml source path and line. Hidden regions keep
            // the last #line mapping (Roslyn semantics); a tree with no
            // directives maps to nothing — empty path, the site is dropped.
            var span = location.GetMappedLineSpan();
            if (string.IsNullOrEmpty(span.Path))
                return;
            Sites.Add(new RawSite(
                key,
                span.Path,
                span.StartLinePosition.Line + 1,
                span.StartLinePosition.Character,
                span.EndLinePosition.Line + 1));
        }
    }
}
