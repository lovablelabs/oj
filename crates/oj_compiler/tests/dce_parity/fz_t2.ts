import { createServerOnlyFn } from "@tanstack/react-start";
export function make() {
  var a = 1;
  var a = 2;
  return createServerOnlyFn(/*<*/() => a/*>*/);
}
