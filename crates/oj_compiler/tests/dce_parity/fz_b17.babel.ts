import { createServerOnlyFn } from "@tanstack/react-start";
export function m() {
  return createServerOnlyFn(undefined) && (() => {
    const a = g();
    return 1;
  })();
}
