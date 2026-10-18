using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CSharp;
using Microsoft.CodeAnalysis.FindSymbols;
using ScipCsharp;
using Xunit;

namespace ScipCsharp.Tests;

/// <summary>
/// Pins how the detached map symbol is paired with its workspace counterpart:
/// the exact overload must win over whichever similar symbol comes first, and
/// an unresolvable pairing must be reported as a fallback, never passed off
/// as a match.
/// </summary>
public class WorkspaceSymbolPickTests
{
    private const string Overloads =
        "class C { public int Add(int a, int b) => a + b; public int Add(int a, int b, int c) => a + b + c; }";

    private const string OtherOverloads =
        "class C { public int Add(int a) => a; public int Add(long a) => 0; }";

    private static CSharpCompilation Compile(string source) =>
        CSharpCompilation.Create(
            "Acme.Core",
            [CSharpSyntaxTree.ParseText(source)],
            references: [MetadataReference.CreateFromFile(typeof(object).Assembly.Location)],
            options: new CSharpCompilationOptions(OutputKind.DynamicallyLinkedLibrary));

    private static List<IMethodSymbol> AddOverloads(Compilation compilation) =>
        compilation.GetTypeByMetadataName("C")!
            .GetMembers("Add")
            .OfType<IMethodSymbol>()
            .OrderBy(m => m.Parameters.Length)
            .ToList();

    [Fact]
    public void PicksTheMatchingOverloadFromTheWorkspaceCompilation()
    {
        var detached = Compile(Overloads);
        var workspace = Compile(Overloads);
        var target = AddOverloads(detached)[1];

        var similar = SymbolFinder.FindSimilarSymbols(target, workspace);
        var picked = ReferenceResolver.PickWorkspaceSymbol(target, similar, out var fellBack);

        Assert.False(fellBack);
        Assert.NotSame(target, picked);
        Assert.Same(workspace.Assembly, picked.ContainingAssembly);
        Assert.Equal(3, Assert.IsAssignableFrom<IMethodSymbol>(picked).Parameters.Length);
    }

    [Fact]
    public void ExactSignatureWinsOverAnEarlierCandidate()
    {
        var target = AddOverloads(Compile(Overloads))[1];
        var workspaceOverloads = AddOverloads(Compile(Overloads));

        var picked = ReferenceResolver.PickWorkspaceSymbol(target, workspaceOverloads, out var fellBack);

        Assert.False(fellBack);
        Assert.Same(workspaceOverloads[1], picked);
    }

    [Fact]
    public void NoCandidatesFallsBackToTheTarget()
    {
        var target = AddOverloads(Compile(Overloads))[1];

        var picked = ReferenceResolver.PickWorkspaceSymbol(target, [], out var fellBack);

        Assert.True(fellBack);
        Assert.Same(target, picked);
    }

    [Fact]
    public void LoneNonExactCandidateIsAccepted()
    {
        var target = AddOverloads(Compile(Overloads))[1];
        var lone = AddOverloads(Compile(OtherOverloads))[0];

        var picked = ReferenceResolver.PickWorkspaceSymbol(target, [lone], out var fellBack);

        Assert.False(fellBack);
        Assert.Same(lone, picked);
    }

    [Fact]
    public void SeveralNonExactCandidatesFallBackToTheTarget()
    {
        var target = AddOverloads(Compile(Overloads))[1];
        var others = AddOverloads(Compile(OtherOverloads));

        var picked = ReferenceResolver.PickWorkspaceSymbol(target, others, out var fellBack);

        Assert.True(fellBack);
        Assert.Same(target, picked);
    }
}
