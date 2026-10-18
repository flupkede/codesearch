namespace Parity.Core;

public sealed class Query<T>(IEnumerable<T> items)
{
    public IEnumerable<T> Items { get; } = items;

    public Query<T> Where(Func<T, bool> predicate) => new(Items.Where(predicate));

    public Query<TResult> Select<TResult>(Func<T, TResult> selector) => new(Items.Select(selector));

    public Query<T> OrderBy<TKey>(Func<T, TKey> key) => new(Items.OrderBy(key));

    public Query<TResult> SelectMany<TCollection, TResult>(
        Func<T, Query<TCollection>> collection, Func<T, TCollection, TResult> result) =>
        new(Items.SelectMany(item => collection(item).Items, result));

    public Query<TResult> Join<TInner, TKey, TResult>(
        Query<TInner> inner, Func<T, TKey> outerKey, Func<TInner, TKey> innerKey, Func<T, TInner, TResult> result) =>
        new(Items.Join(inner.Items, outerKey, innerKey, result));
}

public sealed class Beacon
{
    private EventHandler? _handlers;

    public event EventHandler Pinged
    {
        add => _handlers += value;
        remove => _handlers -= value;
    }

    public void Ping() => _handlers?.Invoke(this, EventArgs.Empty);
}
