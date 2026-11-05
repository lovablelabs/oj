import { createServerOnlyFn } from "@tanstack/react-start";
import { a } from "./m";
export function m() {
  if (x) { hoisted(); }
  return createServerOnlyFn(/*<*/() => hoisted()/*>*/);
  function hoisted() { return a }
}
