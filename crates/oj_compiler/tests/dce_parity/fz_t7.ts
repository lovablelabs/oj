import { createServerOnlyFn } from "@tanstack/react-start";
export function make(obj: any) {
  const {} = obj;
  return createServerOnlyFn(/*<*/() => 1/*>*/);
}
