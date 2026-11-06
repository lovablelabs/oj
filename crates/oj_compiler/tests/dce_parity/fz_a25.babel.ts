import { createServerOnlyFn } from "@tanstack/react-start";
export function m() {
  const {
    a
  } = obj;
  const {
    c,
    d,
    ...e
  } = obj;
  return [a, e, createServerOnlyFn(undefined)];
}
