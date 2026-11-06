import { createServerOnlyFn } from "@tanstack/react-start";
export function make() {
  let cached;
  return createServerOnlyFn(undefined);
}
