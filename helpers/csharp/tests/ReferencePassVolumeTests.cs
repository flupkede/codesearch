using System.Diagnostics;
using System.Text;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CSharp;
using Microsoft.CodeAnalysis.Text;
using ScipCsharp;
using Xunit;
using Xunit.Abstractions;

namespace ScipCsharp.Tests;

/// <summary>
/// Guards the record volume of the single-pass cascade when many types
/// override one framework member: every override of object.ToString lands in
/// one inheritance component, so each ToString site is copied to every
/// override key, exactly as FindReferencesAsync attributes it, and never more.
/// </summary>
public class ReferencePassVolumeTests(ITestOutputHelper output)
{
    private const string ProjectName = "Volume";
    private const int OverrideTypes = 50;
    private const int CallerFiles = 10;
    private const int ObjectCallsPerFile = 5;
    private const int OverrideCallsPerFile = 40;
    private const int PlainCallsPerFile = 5;
    private const int KeysPerOverrideType = 2;
    private const int PlainTypeKeys = 1;

    [Fact]
    public async Task Cascade_FrameworkBridgeMember_RecordVolume()
    {
        using var workspace = new AdhocWorkspace();
        var solution = BuildSolution(workspace);
        var project = solution.Projects.Single();

        var (compilation, _) = await GeneratorSupport.GetCompilationWithGeneratorsAsync(project);
        Assert.NotNull(compilation);
        Assert.Empty(compilation.GetDiagnostics().Where(d => d.Severity == DiagnosticSeverity.Error));

        var definitions = new Dictionary<ISymbol, string>(SymbolEqualityComparer.Default);
        SymbolIndexer.CollectSymbols(compilation.GlobalNamespace, definitions);
        var projectRoot = SymbolIndexer.FindCommonRoot(solution.Projects.Select(p => p.FilePath!));

        var singleSw = Stopwatch.StartNew();
        var pass = new ReferencePass(projectRoot, solution.Projects.Select(p => p.AssemblyName));
        await pass.CollectAsync(compilation);
        var records = pass.Build(definitions);
        singleSw.Stop();

        var probeKey = $"csharp {ProjectName} . Override0#ToString().";
        Assert.Contains(probeKey, definitions.Values);
        var probeRecords = records.Count(r => r.Symbol == probeKey);
        var toStringRecords = records.Count(r => r.Symbol.EndsWith("#ToString().", StringComparison.Ordinal));
        var distinctKeys = records.Select(r => r.Symbol).Distinct().Count();

        var lazySw = Stopwatch.StartNew();
        var lazy = await new ReferenceResolver().BatchFindRefsAsync(solution, [probeKey]);
        lazySw.Stop();
        var lazyResult = Assert.Single(lazy.Results);
        Assert.Empty(lazyResult.Warnings);

        var toStringSites = CallerFiles * (ObjectCallsPerFile + OverrideCallsPerFile + PlainCallsPerFile);
        // Every call statement constructs its own receiver: one type site per ToString site.
        var typeSites = toStringSites;
        var recordCeiling = OverrideTypes * toStringSites + typeSites;
        var expectedKeys = OverrideTypes * KeysPerOverrideType + PlainTypeKeys;
        output.WriteLine($"overrides {OverrideTypes}, ToString call sites {toStringSites}, definitions {definitions.Count}");
        output.WriteLine($"single-pass records total {records.Count} (ToString keys {toStringRecords}), distinct keys {distinctKeys}");
        output.WriteLine($"single-pass records for {probeKey}: {probeRecords}");
        output.WriteLine($"lazy FindReferencesAsync for {probeKey}: {lazyResult.References.Count}");
        output.WriteLine($"elapsed single-pass {singleSw.ElapsedMilliseconds} ms, lazy one key {lazySw.ElapsedMilliseconds} ms");

        Assert.Equal(lazyResult.References.Count, probeRecords);
        Assert.True(records.Count <= recordCeiling,
            $"{records.Count} records exceed the {OverrideTypes} x {toStringSites} cascade plus {typeSites} type sites ({recordCeiling})");
        Assert.Equal(expectedKeys, distinctKeys);
    }

    private static Solution BuildSolution(AdhocWorkspace workspace)
    {
        var root = Path.Combine(AppContext.BaseDirectory, "VolumeProbe");
        var projectId = ProjectId.CreateNewId();
        var solution = workspace.CurrentSolution.AddProject(ProjectInfo.Create(
            projectId,
            VersionStamp.Create(),
            ProjectName,
            ProjectName,
            LanguageNames.CSharp,
            filePath: Path.Combine(root, ProjectName + ".csproj"),
            compilationOptions: new CSharpCompilationOptions(OutputKind.DynamicallyLinkedLibrary),
            metadataReferences: FrameworkReferences()));
        foreach (var (name, text) in Sources())
        {
            solution = solution.AddDocument(
                DocumentId.CreateNewId(projectId), name, SourceText.From(text), filePath: Path.Combine(root, name));
        }
        return solution;
    }

    private static IEnumerable<MetadataReference> FrameworkReferences() =>
        ((string)AppContext.GetData("TRUSTED_PLATFORM_ASSEMBLIES")!)
            .Split(Path.PathSeparator)
            .Where(path => Path.GetFileName(path) is var name
                && (name.StartsWith("System.", StringComparison.Ordinal) || name is "mscorlib.dll" or "netstandard.dll"))
            .Select(path => MetadataReference.CreateFromFile(path));

    private static IEnumerable<(string Name, string Text)> Sources()
    {
        var types = new StringBuilder($"namespace {ProjectName};\n\npublic class Plain\n{{\n}}\n");
        for (var i = 0; i < OverrideTypes; i++)
        {
            types.Append($"\npublic class Override{i}\n{{\n    public override string ToString() => \"{i}\";\n}}\n");
        }
        yield return ("Types.cs", types.ToString());

        for (var file = 0; file < CallerFiles; file++)
        {
            var calls = new StringBuilder(
                $"namespace {ProjectName};\n\npublic static class Callers{file}\n{{\n    public static int Run()\n    {{\n        var total = 0;\n");
            for (var k = 0; k < ObjectCallsPerFile; k++)
            {
                calls.Append($"        object boxed{k} = new Override{(file + k) % OverrideTypes}();\n");
                calls.Append($"        total += boxed{k}.ToString().Length;\n");
            }
            for (var k = 0; k < OverrideCallsPerFile; k++)
            {
                calls.Append($"        total += new Override{(file * OverrideCallsPerFile + k) % OverrideTypes}().ToString().Length;\n");
            }
            for (var k = 0; k < PlainCallsPerFile; k++)
            {
                calls.Append("        total += new Plain().ToString().Length;\n");
            }
            calls.Append("        return total;\n    }\n}\n");
            yield return ($"Callers{file}.cs", calls.ToString());
        }
    }
}
