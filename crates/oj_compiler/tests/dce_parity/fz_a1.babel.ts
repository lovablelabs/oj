import { createServerOnlyFn } from "@tanstack/react-start";
export function m() {
  const x = createServerOnlyFn(undefined);
  const a = g();
  return x;
}
