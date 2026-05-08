using SmallSolution.Library;

namespace SmallSolution.App;

public class Program
{
    public static void Main(string[] args)
    {
        var calc = new Calculator();
        int sum = calc.Add(3, 4);
        int diff = calc.Subtract(10, 3);
        int product = calc.Multiply(2, 5);

        string formatted = calc.FormatResult(sum);
        Console.WriteLine(formatted);

        // Use the extension method
        int result = calc.Compute(Operation.Add, 7, 2);
        Console.WriteLine(result);
    }
}
