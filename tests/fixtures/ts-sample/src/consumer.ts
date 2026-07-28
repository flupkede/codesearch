import { add } from "./math";

export function sumThree(a: number, b: number, c: number): number {
  return add(add(a, b), c);
}
