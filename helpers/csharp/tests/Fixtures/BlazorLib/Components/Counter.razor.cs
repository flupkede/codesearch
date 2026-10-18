namespace BlazorLib.Components;

// The code-behind half of the partial: `count` and `Increment` live in the
// razor file and only come into existence once the Razor source generator
// has run — exactly the members that produced the false CS0103 cascade.
public partial class Counter
{
    public string Label => $"count: {count}";

    public void BumpTwice()
    {
        Increment();
        Increment();
    }
}
