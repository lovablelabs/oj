import { createServerOnlyFn } from "@tanstack/react-start";
export function make() {
  function req(req: number) { return req; }
  return createServerOnlyFn(/*<*/() => req(1)/*>*/);
}
