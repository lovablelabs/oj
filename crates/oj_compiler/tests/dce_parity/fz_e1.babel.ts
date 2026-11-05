import { createServerOnlyFn } from "@tanstack/react-start";
export function m() {
  foo();
  [1].forEach(x => x);
  return createServerOnlyFn(undefined);
}
