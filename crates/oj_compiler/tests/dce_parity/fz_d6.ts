import { createServerOnlyFn } from "@tanstack/react-start";
import { s } from "./m";
export function m() {
  const h = s
  ;[1].forEach(x => x)
  return createServerOnlyFn(/*<*/() => h/*>*/)
}
