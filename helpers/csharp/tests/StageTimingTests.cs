using System.Globalization;
using ScipCsharp;
using Xunit;

namespace ScipCsharp.Tests;

public class StageTimingTests
{
    [Fact]
    public void FormatIsPrefixStageSeconds() =>
        Assert.Equal("[TIMING] compile 12.3s", StageTiming.Format("compile", TimeSpan.FromSeconds(12.34)));

    [Fact]
    public void FormatIsCultureInvariant()
    {
        var previous = CultureInfo.CurrentCulture;
        try
        {
            CultureInfo.CurrentCulture = new CultureInfo("nl-BE");
            Assert.Equal("[TIMING] total 1.5s", StageTiming.Format("total", TimeSpan.FromSeconds(1.5)));
        }
        finally
        {
            CultureInfo.CurrentCulture = previous;
        }
    }

    [Fact]
    public void LineNeverTripsTheWarningCapture()
    {
        var line = StageTiming.Format("workspace_open", TimeSpan.Zero);
        Assert.False(line.StartsWith("[WARN] ", StringComparison.Ordinal));
        Assert.DoesNotContain("[Failure]", line);
    }
}
