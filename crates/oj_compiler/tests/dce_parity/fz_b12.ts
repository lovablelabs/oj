import { createServerOnlyFn } from "@tanstack/react-start";
export function m() {
  const { a = side(), b } = obj;
  return [b, createServerOnlyFn(/*<*/() => a/*>*/)];
}
