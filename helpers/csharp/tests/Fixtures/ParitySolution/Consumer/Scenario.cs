using Parity.Core;
using Parity.Services;

namespace Parity.Consumer;

[Audited]
[AuditedAttribute("full name")]
public static class Scenario
{
    public static double Run()
    {
        var store = new Store<Circle>();
        IRepository<Circle> repository = store;
        var service = new OrderService(repository);
        Circle unit = new();
        var big = new Circle(3.0);
        service.Place(big);
        service.Place(unit, 2);
        repository.Save(unit);
        store.Save(big);
        new CircleStore().Save(unit);
        new ArchiveRepository().Save(new Square(2.0));

        var found = repository.Find(0);
        var count = store.Count + repository.Count;
        store.Changed += (_, _) => count++;
        var area = store.Map(c => c.Area());
        var first = store[0];

        Shape picked = TextTools.Pick<Shape>(big, unit, true);
        var number = TextTools.Pick(1, 2, false);
        var shout = TextTools.Shout("x");

        var ledger = Ledger.Create();
        ledger.Record(5);
        ledger.Total = 3;
        store.Version = ledger.Total;

        big.Name = "renamed";
        var name = big.Name;
        var names = new[] { nameof(Circle), nameof(Shape.Area), nameof(Shape.Name), nameof(TextTools.Pad) };

        object boxed = big;
        var text = picked.ToString() + boxed.ToString() + picked.Describe();
        IClock clock = new FixedClock();

        var palette = new Palette { "red", "blue" };
        foreach (var color in palette)
        {
            text += color;
        }
        var (head, size) = palette;

        return area + found!.Radius + first.Area() + number + shout.Length + name.Length
            + names.Length + text.Length + clock.Now.Year + head.Length + size;
    }
}
