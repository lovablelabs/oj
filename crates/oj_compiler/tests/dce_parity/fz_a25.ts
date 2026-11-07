import { createServerOnlyFn } from "@tanstack/react-start";
export function m() {
  const { a, b, } = obj;
  const { c, d, ...e } = obj;
  return [a, e, createServerOnlyFn(/*<*/() => b + c + d/*>*/)];
}
