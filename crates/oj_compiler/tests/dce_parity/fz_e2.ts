import { createServerOnlyFn } from "@tanstack/react-start";
import { s } from "./m";
export function m() {
  let r = foo()
  function h() { return s }
  ;(r)
  return createServerOnlyFn(/*<*/() => h()/*>*/)
}
