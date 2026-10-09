using System.Collections.Concurrent;
using System.Reflection;
using System.Runtime.Loader;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CSharp;
using Microsoft.CodeAnalysis.Diagnostics;
using Microsoft.CodeAnalysis.Text;

namespace ScipCsharp;

/// <summary>
/// Why generators run here: Project.GetCompilationAsync() never executes source
/// generators. A workspace (design-time) build stages the generator assemblies,
/// the .razor files (AdditionalDocuments) and GeneratedMSBuildEditorConfig.editorconfig,
/// but nothing drives a GeneratorDriver. On Blazor projects the code-behind
/// partial then cannot see the @code members — CS0103/CS0115 cascades that make
/// the index warnings fire although `dotnet build` is clean.
/// </summary>
public enum GeneratorRunOutcome
{
    /// No generator pipeline attempted (no C# compilation, no generators declared).
    NotRun,
    /// Generators ran; the returned compilation includes their output.
    Ran,
    /// The generator pipeline failed — a tool-side gap, not a user error.
    Failed,
}

public static class GeneratorSupport
{
    // Recovery is expensive (ALC load + reflection per analyzer); the resulting
    // generator instances are stateless and reusable across projects and drivers.
    private static readonly ConcurrentDictionary<string, ISourceGenerator[]> RecoveryCache =
        new(StringComparer.OrdinalIgnoreCase);

    /// <summary>
    /// Compilation for symbol collection: plain compilation with all declared
    /// source generators run over it. On any failure returns the plain
    /// compilation with <see cref="GeneratorRunOutcome.Failed"/> so callers can
    /// downgrade the resulting diagnostics to [INFO] instead of [WARN] —
    /// generator failures are a tool-side gap the user cannot fix.
    /// </summary>
    public static async Task<(Compilation? Compilation, GeneratorRunOutcome Outcome)>
        GetCompilationWithGeneratorsAsync(Project project)
    {
        var compilation = await project.GetCompilationAsync().ConfigureAwait(false);
        if (compilation is not CSharpCompilation csharp)
            return (compilation, GeneratorRunOutcome.NotRun);
        if (project.Language != LanguageNames.CSharp || !csharp.SyntaxTrees.Any())
            return (compilation, GeneratorRunOutcome.NotRun);

        try
        {
            var generators = CollectGenerators(project);
            if (generators.Count == 0)
                return (compilation, GeneratorRunOutcome.NotRun);

            var additionalTexts = await CollectAdditionalTextsAsync(project).ConfigureAwait(false);
            var optionsProvider = await BuildOptionsProviderAsync(project).ConfigureAwait(false);
            var parseOptions = (CSharpParseOptions)csharp.SyntaxTrees.First().Options;

            var driver = CSharpGeneratorDriver.Create(generators, additionalTexts, parseOptions, optionsProvider);
            driver.RunGeneratorsAndUpdateCompilation(csharp, out var output, out _);
            return (output, GeneratorRunOutcome.Ran);
        }
        catch (Exception ex)
        {
            Console.Error.WriteLine(
                $"[INFO] Source generators could not run for {project.Name}: {ex.GetType().Name}: {ex.Message}. " +
                "Generated-only declarations (e.g. Razor components) may be missing from this project's symbols.");
            return (compilation, GeneratorRunOutcome.Failed);
        }
    }

    /// <summary>
    /// Generators declared by the project. Roslyn's generator discovery is
    /// tried first; references where it returns nothing are loaded through a
    /// tolerant loader — the Roslyn 4.x version guard throws (NRE on
    /// version-less assembly references) for analyzers built against a newer
    /// Roslyn than the host pins, which is exactly the Razor SDK's case.
    /// </summary>
    private static List<ISourceGenerator> CollectGenerators(Project project)
    {
        var generators = new List<ISourceGenerator>();
        foreach (var reference in project.AnalyzerReferences.OfType<AnalyzerFileReference>())
        {
            System.Collections.Immutable.ImmutableArray<ISourceGenerator> viaRoslyn;
            try { viaRoslyn = reference.GetGenerators(LanguageNames.CSharp); }
            catch { viaRoslyn = System.Collections.Immutable.ImmutableArray<ISourceGenerator>.Empty; }
            if (viaRoslyn.Length > 0)
            {
                generators.AddRange(viaRoslyn);
                continue;
            }

            generators.AddRange(RecoveryCache.GetOrAdd(reference.FullPath, static path => RecoverGenerators(path)));
        }
        return generators;
    }

    private static ISourceGenerator[] RecoverGenerators(string analyzerPath)
    {
        if (!File.Exists(analyzerPath))
            return [];
        try
        {
            var loader = new RecoveryLoader(analyzerPath);
            var assembly = loader.LoadFromAssemblyPath(analyzerPath);
            Type[] types;
            try { types = assembly.GetTypes(); }
            catch (ReflectionTypeLoadException rtle)
            {
                types = rtle.Types.Where(t => t is not null).ToArray()!;
            }

            var found = new List<ISourceGenerator>();
            foreach (var type in types)
            {
                if (!type.GetCustomAttributesData().Any(a => a.AttributeType.Name == "GeneratorAttribute"))
                    continue;
                object? instance;
                try { instance = Activator.CreateInstance(type); }
                catch { continue; }
                if (instance is IIncrementalGenerator incremental)
                    found.Add(Microsoft.CodeAnalysis.GeneratorExtensions.AsSourceGenerator(incremental));
                else if (instance is ISourceGenerator classic)
                    found.Add(classic);
            }
            return [.. found];
        }
        catch
        {
            return [];
        }
    }

    /// <summary>
    /// Binds host assemblies by NAME so generator assemblies built against a
    /// newer Roslyn share one type identity with the host's pinned version.
    /// Dependencies not listed in the analyzer's deps.json (SDK-internal
    /// project references) are probed next to the analyzer itself.
    /// </summary>
    private sealed class RecoveryLoader : AssemblyLoadContext
    {
        private readonly string _analyzerDir;
        private readonly AssemblyDependencyResolver _resolver;

        public RecoveryLoader(string analyzerPath)
            : base($"scip-csharp-generator-{Path.GetFileName(analyzerPath)}", isCollectible: false)
        {
            _analyzerDir = Path.GetDirectoryName(analyzerPath)!;
            _resolver = new AssemblyDependencyResolver(analyzerPath);
        }

        protected override Assembly? Load(AssemblyName assemblyName)
        {
            var name = assemblyName.Name;
            if (name is null)
                return null;
            foreach (var asm in Default.Assemblies)
            {
                if (string.Equals(asm.GetName().Name, name, StringComparison.OrdinalIgnoreCase))
                    return asm;
            }
            var path = _resolver.ResolveAssemblyToPath(assemblyName) ?? ProbeAnalyzerDir(name);
            return path is not null ? LoadFromAssemblyPath(path) : null;
        }

        private string? ProbeAnalyzerDir(string name)
        {
            var candidate = Path.Combine(_analyzerDir, name + ".dll");
            return File.Exists(candidate) ? candidate : null;
        }
    }

    private static async Task<List<AdditionalText>> CollectAdditionalTextsAsync(Project project)
    {
        var texts = new List<AdditionalText>();
        foreach (var doc in project.AdditionalDocuments)
            texts.Add(new DocText(doc.FilePath!, await doc.GetTextAsync().ConfigureAwait(false)));
        return texts;
    }

    private static async Task<AnalyzerConfigOptionsProvider> BuildOptionsProviderAsync(Project project)
    {
        var documents = new List<(string Path, string Text)>();
        foreach (var doc in project.AnalyzerConfigDocuments)
            documents.Add((doc.FilePath!, (await doc.GetTextAsync().ConfigureAwait(false)).ToString()));
        var options = new EditorConfigOptions(documents);
        return new PathOptionsProvider(options.Global, path => options.ForPath(path));
    }
}

/// <summary>AnalyzerConfigOptions backed by a case-insensitive map; MSBuild
/// editorconfigs repeat keys, so last wins.</summary>
internal sealed class DictOptions : AnalyzerConfigOptions
{
    private readonly Dictionary<string, string> values;

    public DictOptions(IEnumerable<KeyValuePair<string, string>> entries)
    {
        values = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        foreach (var (key, value) in entries)
            values[key] = value;
    }

    public override bool TryGetValue(string key, out string value) => values.TryGetValue(key, out value!);
    public override IEnumerable<string> Keys => values.Keys;

    internal static readonly DictOptions Empty = new([]);
}

/// <summary>Parses MSBuild editorconfig documents (is_global section plus
/// [path] sections) into per-path options for source generators.</summary>
internal sealed class EditorConfigOptions
{
    private readonly Dictionary<string, DictOptions> byPath = new(StringComparer.OrdinalIgnoreCase);
    private readonly DictOptions global;

    public EditorConfigOptions(IEnumerable<(string Path, string Text)> documents)
    {
        var globalEntries = new List<KeyValuePair<string, string>>();
        foreach (var (docPath, text) in documents)
        {
            string? section = null;
            var sectionEntries = new List<KeyValuePair<string, string>>();
            foreach (var rawLine in text.Split('\n'))
            {
                var line = rawLine.Trim();
                if (line.Length == 0 || line.StartsWith('#') || line.StartsWith(';'))
                    continue;
                if (line.StartsWith('[') && line.EndsWith(']'))
                {
                    FlushSection(section, sectionEntries);
                    section = Normalize(line[1..^1]);
                    sectionEntries = new List<KeyValuePair<string, string>>();
                }
                else if (section is null)
                {
                    var kv = SplitKv(line);
                    if (kv is not null && !kv.Value.Key.Equals("is_global", StringComparison.OrdinalIgnoreCase))
                        globalEntries.Add(kv.Value);
                }
                else
                {
                    var kv = SplitKv(line);
                    if (kv is not null)
                        sectionEntries.Add(kv.Value);
                }
            }
            FlushSection(section, sectionEntries);
        }
        global = new DictOptions(globalEntries);
    }

    private void FlushSection(string? section, List<KeyValuePair<string, string>> entries)
    {
        if (section is not null && entries.Count > 0)
            byPath[section] = new DictOptions(entries);
    }

    private static string Normalize(string path) => path.Replace('\\', '/');

    private static KeyValuePair<string, string>? SplitKv(string line)
    {
        var idx = line.IndexOf('=');
        if (idx < 0)
            return null;
        return new KeyValuePair<string, string>(line[..idx].Trim(), line[(idx + 1)..].Trim());
    }

    public AnalyzerConfigOptions Global => global;

    public AnalyzerConfigOptions ForPath(string path) =>
        byPath.TryGetValue(Normalize(path), out var opts) ? opts : DictOptions.Empty;
}

internal sealed class PathOptionsProvider(AnalyzerConfigOptions global, Func<string, AnalyzerConfigOptions> byPath)
    : AnalyzerConfigOptionsProvider
{
    public override AnalyzerConfigOptions GlobalOptions => global;
    public override AnalyzerConfigOptions GetOptions(SyntaxTree tree) => byPath(tree.FilePath);
    public override AnalyzerConfigOptions GetOptions(AdditionalText file) => byPath(file.Path);
}

internal sealed class DocText(string path, SourceText text) : AdditionalText
{
    public override string Path { get; } = path;

    public override SourceText GetText(CancellationToken cancellationToken = default) => text;
}
