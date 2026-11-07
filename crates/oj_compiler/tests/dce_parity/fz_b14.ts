import { createServerOnlyFn } from "@tanstack/react-start";
import { a } from "./m";
export function m() {
  var h = a;
  { var h; }
  return createServerOnlyFn(/*<*/() => h/*>*/);
}
