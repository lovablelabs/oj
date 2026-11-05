import { createServerOnlyFn } from "@tanstack/react-start";
export function make(obj: any) {
  const {
    ...rest
  } = obj;
  return [rest, createServerOnlyFn(undefined)];
}
