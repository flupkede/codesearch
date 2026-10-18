using Parity.Core;

namespace Parity.Services;

[Audited]
public class OrderService
{
    private readonly IRepository<Circle> _repository;
    private readonly Ledger _ledger = Ledger.Create();

    public OrderService(IRepository<Circle> repository)
    {
        _repository = repository;
        _repository.Changed += OnChanged;
    }

    public int ChangeCount { get; private set; }

    [Audited("save")]
    public void Place(Circle circle)
    {
        _repository.Save(circle);
        _ledger.Record(1);
        var label = circle.Name.Pad(10);
        Console.WriteLine(TextTools.Pad(label, 12, '*'));
    }

    [Audited(Reason = "bulk")]
    public int Place(Circle circle, int copies)
    {
        for (var i = 0; i < copies; i++)
        {
            Place(circle);
        }
        return _repository.Count;
    }

    private void OnChanged(object? sender, EventArgs e) => ChangeCount++;
}

public class CircleStore : Store<Circle>
{
    public override void Save(Circle item)
    {
        base.Save(item);
        Version += TextTools.Shout(item.Name).Length;
    }
}

public sealed class ArchiveRepository : IRepository<Shape>
{
    private readonly List<Shape> _shapes = [];

    public int Count => _shapes.Count;

    public event EventHandler? Changed;

    public Shape? Find(int id) => _shapes.ElementAtOrDefault(id);

    public void Save(Shape item)
    {
        _shapes.Add(item);
        Changed?.Invoke(this, EventArgs.Empty);
    }
}

public sealed class FixedClock : IClock
{
    DateTime IClock.Now => new(2020, 1, 1);
}
