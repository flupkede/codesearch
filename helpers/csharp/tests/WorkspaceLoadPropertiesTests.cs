using Xunit;
using ScipCsharp;

namespace ScipCsharp.Tests;

/// <summary>
/// Pins the workspace load properties. The contract that matters: the
/// symbol load NEVER escalates warnings to errors — a repo with
/// TreatWarningsAsErrors (own settings or Directory.Build.props) must still
/// load its projects, because benign load-time diagnostics (NU1603 class)
/// would otherwise drop the project and every cross-project reference to it.
/// Reverting any of the three demotion properties must fail this test.
/// </summary>
public class WorkspaceLoadPropertiesTests
{
    [Fact]
    public void Load_NeverEscalatesWarningsToErrors()
    {
        var properties = WorkspaceLoad.GlobalProperties();

        Assert.Equal("false", properties["TreatWarningsAsErrors"]);
        Assert.Equal("", properties["WarningsAsErrors"]);
        Assert.Equal("false", properties["MSBuildTreatWarningsAsErrors"]);
    }

    [Fact]
    public void Load_KeepsDesignTimeBuildContract()
    {
        var properties = WorkspaceLoad.GlobalProperties();

        Assert.Equal("true", properties["BuildingInsideVisualStudio"]);
        Assert.Equal("true", properties["DesignTimeBuild"]);
        Assert.Equal("true", properties["SkipCompilerExecution"]);
    }
}
