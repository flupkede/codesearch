using System.Globalization;

namespace ScipCsharp;

/// <summary>
/// Stage timings on stderr for the `index` command. The prefix is `[TIMING] `
/// BY DESIGN: the Rust capture (`is_helper_warning_line`) keys on
/// `[WARN] `/`[Failure]`, so timings never enter the index-warnings channel.
/// Compiled into both scip-csharp and the test project via a linked Compile item.
/// </summary>
internal static class StageTiming
{
    internal const string Prefix = "[TIMING] ";

    internal static string Seconds(TimeSpan elapsed) =>
        elapsed.TotalSeconds.ToString("F1", CultureInfo.InvariantCulture) + "s";

    internal static string Format(string stage, TimeSpan elapsed) =>
        $"{Prefix}{stage} {Seconds(elapsed)}";

    internal static void Log(string stage, TimeSpan elapsed) =>
        Console.Error.WriteLine(Format(stage, elapsed));
}
