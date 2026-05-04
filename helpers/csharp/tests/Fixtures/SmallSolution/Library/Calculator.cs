namespace SmallSolution.Library;

/// <summary>
/// A simple calculator class used for testing symbol indexing.
/// </summary>
public class Calculator
{
    public int Add(int a, int b)
    {
        return a + b;
    }

    public int Subtract(int a, int b)
    {
        return a - b;
    }

    public int Multiply(int a, int b)
    {
        return a * b;
    }

    public double Divide(int a, int b)
    {
        if (b == 0)
            throw new DivideByZeroException("Cannot divide by zero");
        return (double)a / b;
    }

    public string FormatResult(int value)
    {
        return $"Result: {value}";
    }
}

public enum Operation
{
    Add,
    Subtract,
}

public static class CalculatorExtensions
{
    public static int Compute(this Calculator calc, Operation op, int a, int b)
    {
        return op switch
        {
            Operation.Add => calc.Add(a, b),
            Operation.Subtract => calc.Subtract(a, b),
            _ => throw new ArgumentOutOfRangeException(nameof(op)),
        };
    }
}
