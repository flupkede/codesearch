using Parity.Core;

namespace Parity.Consumer;

public static class Pipelines
{
    public static int Run()
    {
        var circles = new Query<Circle>(new[] { new Circle(1.0), new Circle(2.0) });
        var squares = new Query<Square>(new[] { new Square(2.0) });

        // FindReferencesAsync reports no reference for query clauses bound to the user-defined query methods.
        var names = from c in circles
                    where c.Radius > 1
                    orderby c.Radius
                    select c.Name;
        var pairs = from c in circles
                    join s in squares on c.Name equals s.Name
                    select c.Radius + s.Side;
        var products = from c in circles
                       from s in squares
                       select c.Radius * s.Side;

        var beacon = new Beacon();
        var pings = 0;
        EventHandler handler = (_, _) => pings++;
        // `+=` and `-=` count for the event only, never for its explicit add/remove accessors.
        beacon.Pinged += handler;
        beacon.Ping();
        beacon.Pinged -= handler;

        return names.Items.Count() + pairs.Items.Count() + products.Items.Count() + pings;
    }
}
