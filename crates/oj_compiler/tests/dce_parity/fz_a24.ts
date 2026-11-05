import { createServerOnlyFn } from "@tanstack/react-start";
export function m() {
  let a = g(), /* x */ b = g(), // y
    c = 3;
  return [b, createServerOnlyFn(/*<*/() => a + c/*>*/)];
}
