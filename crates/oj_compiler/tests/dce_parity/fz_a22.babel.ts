import { createServerOnlyFn } from "@tanstack/react-start";
export function m() {
  for (;;) {
    return createServerOnlyFn(undefined);
  }
}
