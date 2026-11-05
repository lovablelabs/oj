import { createServerOnlyFn } from "@tanstack/react-start";
export function make() {
  function k() {
    const a = side();
  }
  return [k, createServerOnlyFn(undefined)];
}
