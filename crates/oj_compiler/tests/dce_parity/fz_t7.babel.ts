import { createServerOnlyFn } from "@tanstack/react-start";
export function make(obj: any) {
  return createServerOnlyFn(undefined);
}
