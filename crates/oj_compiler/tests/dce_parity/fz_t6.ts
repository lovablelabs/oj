import { createServerOnlyFn } from "@tanstack/react-start";
export function make(obj: any) {
  const { a = 1, ...rest } = obj;
  return [rest, createServerOnlyFn(/*<*/() => a/*>*/)];
}
