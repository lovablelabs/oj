import { createServerOnlyFn } from "@tanstack/react-start";
export function m() {
  const { ...e } = obj;
  const [...f] = obj;
  return createServerOnlyFn(/*<*/() => e + f/*>*/);
}
