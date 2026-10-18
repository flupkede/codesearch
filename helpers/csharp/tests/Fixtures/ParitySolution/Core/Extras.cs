namespace Parity.Core;

public record Point(int X, int Y)
{
    public int Sum => X + Y;
}

public delegate void Notify(string message);

public enum Tone
{
    Low,
    High,
}

public static class Outer
{
    public sealed class Inner
    {
        public Tone Level { get; init; } = Tone.Low;
    }
}

public sealed class Session : IDisposable
{
    public bool Closed { get; private set; }

    public void Dispose() => Closed = true;
}

public static class Measure
{
    public static double Total<T>(IEnumerable<T> shapes) where T : Shape => shapes.Sum(s => s.Area());
}
