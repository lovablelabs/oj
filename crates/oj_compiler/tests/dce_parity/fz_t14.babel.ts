import { createServerOnlyFn } from "@tanstack/react-start";
export function make() {
  for (let i = 0; i < 1; i++) {
    createServerOnlyFn(undefined);
  }
}
