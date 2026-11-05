import { createServerOnlyFn } from "@tanstack/react-start";
export function m() {
  [1].forEach(x => x);
  return createServerOnlyFn(undefined);
}
