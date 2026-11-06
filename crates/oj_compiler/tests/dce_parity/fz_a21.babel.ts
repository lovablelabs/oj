import { createServerOnlyFn } from "@tanstack/react-start";
export function m() {
  for (var i = 0; i < 1; i++) {}
  return createServerOnlyFn(undefined);
}
