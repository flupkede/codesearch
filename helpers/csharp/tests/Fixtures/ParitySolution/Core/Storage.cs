namespace Parity.Core;

public interface IRepository<T>
{
    int Count { get; }

    event EventHandler? Changed;

    T? Find(int id);

    void Save(T item);
}

public interface IClock
{
    DateTime Now { get; }
}

public class Store<T> : IRepository<T>
{
    private readonly Dictionary<int, T> _items = new();

    internal int Version;

    public event EventHandler? Changed;

    public int Count => _items.Count;

    public T this[int id] => _items[id];

    public T? Find(int id) => _items.TryGetValue(id, out var item) ? item : default;

    public virtual void Save(T item)
    {
        _items[_items.Count] = item;
        Version++;
        Changed?.Invoke(this, EventArgs.Empty);
    }

    public TResult Map<TResult>(Func<T, TResult> selector) => selector(_items[0]);
}

public static class TextTools
{
    /// <summary>Overload of <see cref="Pad(string, int, char)"/>.</summary>
    public static string Pad(this string value, int width) => value.PadLeft(width);

    public static string Pad(this string value, int width, char fill) => value.PadLeft(width, fill);

    public static T Pick<T>(T first, T second, bool takeFirst) => takeFirst ? first : second;

    internal static string Shout(string value) => value.ToUpperInvariant();
}

public sealed class Palette : IEnumerable<string>
{
    private readonly List<string> _colors = [];

    public void Add(string color) => _colors.Add(color);

    public IEnumerator<string> GetEnumerator() => _colors.GetEnumerator();

    System.Collections.IEnumerator System.Collections.IEnumerable.GetEnumerator() => GetEnumerator();

    public void Deconstruct(out string first, out int count)
    {
        first = _colors[0];
        count = _colors.Count;
    }
}
