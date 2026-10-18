using Parity.Core;
using Geometry = Parity.Core.Circle;
using static Parity.Core.TextTools;

namespace Parity.Consumer;

public static class Extras
{
    public static int Run(object input)
    {
        Geometry alias = new Geometry(2.0);
        var loud = Shout("quiet");
        var point = new Point(1, 2);
        var moved = point with { X = 5 };
        var (x, y) = moved;
        Notify notify = message => loud += message;
        notify("!");
        var inner = new Outer.Inner { Level = Tone.High };
        var kind = typeof(Square);
        var fallback = default(Circle);
        var isBig = input is Circle { Radius: > 1 };
        var cast = (Shape)alias;
        using (var session = new Session())
        {
            loud += session.Closed;
        }
        IDisposable disposable = new Session();
        disposable.Dispose();

        return x + y + moved.Sum + loud.Length + (int)inner.Level + kind.Name.Length
            + (fallback is null ? 0 : 1) + (isBig ? 1 : 0) + (int)Measure.Total(new[] { cast });
    }
}
