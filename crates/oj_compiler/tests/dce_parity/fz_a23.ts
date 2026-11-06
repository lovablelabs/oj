import { createServerOnlyFn } from "@tanstack/react-start";
export function m() {
  let a = 1, b = g(), c = 3;
  return [a, c, createServerOnlyFn(/*<*/() => b/*>*/)];
}
