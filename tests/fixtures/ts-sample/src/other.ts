import { add } from "./math";

export function printSum(a: number, b: number): void {
  const result = add(a, b);
  console.log(result);
}
