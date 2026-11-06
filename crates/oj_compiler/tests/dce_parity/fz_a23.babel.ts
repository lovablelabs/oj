import { createServerOnlyFn } from "@tanstack/react-start";
export function m() {
  let a = 1,
    c = 3;
  return [a, c, createServerOnlyFn(undefined)];
}
