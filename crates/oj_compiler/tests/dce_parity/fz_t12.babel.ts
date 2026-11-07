import { createServerOnlyFn } from "@tanstack/react-start";
export function make() {
  return [createServerOnlyFn(undefined)];
}
