[assembly: System.Runtime.CompilerServices.InternalsVisibleTo("Services")]
[assembly: System.Runtime.CompilerServices.InternalsVisibleTo("Consumer")]

namespace Parity.Core;

/// <summary>Base of <see cref="Circle"/>; see <see cref="Area"/>.</summary>
public abstract class Shape
{
    protected Shape(string name)
    {
        Name = name;
    }

    public string Name { get; set; }

    public abstract double Area();

    public virtual string Describe() => $"{Name}: {Area()}";

    public override string ToString() => Describe();
}

public class Circle : Shape
{
    public Circle(double radius) : base("circle")
    {
        Radius = radius;
    }

    public Circle() : this(1.0)
    {
    }

    public double Radius { get; }

    public override double Area() => Math.PI * Radius * Radius;
}

public sealed class Square(double side) : Shape("square")
{
    public double Side { get; } = side;

    public override double Area() => Side * Side;

    public override string Describe() => "square " + base.Describe();
}

[AttributeUsage(AttributeTargets.Class | AttributeTargets.Method, AllowMultiple = true)]
public sealed class AuditedAttribute : Attribute
{
    public AuditedAttribute()
    {
    }

    public AuditedAttribute(string reason)
    {
        Reason = reason;
    }

    public string? Reason { get; set; }
}

internal sealed class Ledger
{
    internal int Total;

    internal void Record(int amount) => Total += amount;

    internal static Ledger Create() => new();
}
