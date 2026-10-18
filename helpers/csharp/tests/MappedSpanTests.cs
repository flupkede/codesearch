using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CSharp;
using Microsoft.CodeAnalysis.CSharp.Syntax;
using Xunit;

namespace ScipCsharp.Tests;

/// <summary>
/// The Razor mapping contract: source-generated trees carry #line directives
/// that resolve to the .razor/.cshtml source path and line — the same
/// mechanism VS's ctrl-click uses. The helper must emit mapped paths/lines
/// (GetMappedLineSpan) and drop #line-hidden sites of generated trees.
/// </summary>
public class MappedSpanTests
{
    private const string Generated = """
        #line 7 "Components/Plan/ContentPlanEdit.razor"
        public class MappedWidget { }
        #line hidden
        private sealed class HiddenWidget { }
        """;

    private static readonly SyntaxTree Tree = CSharpSyntaxTree.ParseText(Generated);

    private static Location ClassLocation(string name)
    {
        var root = Tree.GetRoot();
        var decl = root.DescendantNodes().OfType<ClassDeclarationSyntax>()
            .First(c => c.Identifier.ValueText == name);
        return decl.Identifier.GetLocation();
    }

    [Fact]
    public void MappedSpan_ResolvesLineDirectiveToRazorSource()
    {
        var mapped = ClassLocation("MappedWidget").GetMappedLineSpan();
        Assert.True(mapped.HasMappedPath, "the #line directive must mark the path as mapped");
        Assert.Equal("Components/Plan/ContentPlanEdit.razor", mapped.Path);
        Assert.Equal(7, mapped.StartLinePosition.Line + 1);
    }

    [Fact]
    public void LocationToRange_UsesTheMappedLine()
    {
        var range = SymbolIndexer.LocationToRange(ClassLocation("MappedWidget"));
        Assert.Equal(7, range[0]);
    }

    [Fact]
    public void HiddenRegion_KeepsTheLastMappedPath()
    {
        // Roslyn semantics: #line hidden continues the last #line mapping —
        // the site stays attributed to the .razor source (approximate line),
        // never to the generated tree's virtual path.
        var mapped = ClassLocation("HiddenWidget").GetMappedLineSpan();
        Assert.True(mapped.HasMappedPath);
        Assert.Equal("Components/Plan/ContentPlanEdit.razor", mapped.Path);
    }

    [Fact]
    public void OrdinaryFile_KeepsIdentityMapping()
    {
        var tree = CSharpSyntaxTree.ParseText(
            "class Ordinary { }\n",
            path: @"C:\repo\src\Ordinary.cs");
        var decl = tree.GetRoot().DescendantNodes().OfType<ClassDeclarationSyntax>().First();
        var mapped = decl.Identifier.GetLocation().GetMappedLineSpan();
        Assert.False(mapped.HasMappedPath, "no #line directives — identity mapping");
        Assert.Equal(@"C:\repo\src\Ordinary.cs", mapped.Path);
        Assert.Equal(1, mapped.StartLinePosition.Line + 1);
    }
}
