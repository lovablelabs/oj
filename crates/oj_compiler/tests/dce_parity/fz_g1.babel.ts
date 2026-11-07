import { createServerOnlyFn } from "@tanstack/react-start";
export function m() {
  const x = createServerOnlyFn(undefined);
  return x;
}
