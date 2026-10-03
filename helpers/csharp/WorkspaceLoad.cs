namespace ScipCsharp;

/// <summary>
/// Global MSBuild properties for every workspace load. Non-negotiables:
/// DesignTimeBuild + SkipCompilerExecution and NO warnings-as-errors — a
/// repo that escalates warnings (own settings or Directory.Build.props)
/// would turn benign load-time diagnostics (the NU1603 version-unification
/// class) into MSBuild project-load failures, dropping the project — and
/// every cross-project reference to it — from the index. Everything else
/// about the project keeps its own settings; we only refuse the escalation.
/// Lives outside Program.cs so the test assembly can compile it in
/// (Program carries the Main entrypoint and is deliberately excluded there).
/// </summary>
internal static class WorkspaceLoad
{
    internal static System.Collections.Generic.Dictionary<string, string> GlobalProperties() => new()
    {
        // Tell Roslyn to skip projects it cannot load instead of crashing.
        { "BuildingInsideVisualStudio", "true" },
        // Design-time build: prevents auto-generated files in obj/ (e.g.
        // .AssemblyAttributes.cs, .AssemblyInfo.cs) from being included as
        // explicit Compile items. Without this, SDK-style projects produce
        // duplicate Compile items (auto-include + obj/ generated), which
        // causes MSBuildWorkspace to fail loading the project.
        { "DesignTimeBuild", "true" },
        { "SkipCompilerExecution", "true" },
        // Warnings stay warnings during the load — never fatal (NU1603 etc.).
        { "TreatWarningsAsErrors", "false" },
        { "WarningsAsErrors", "" },
        { "MSBuildTreatWarningsAsErrors", "false" },
    };
}
