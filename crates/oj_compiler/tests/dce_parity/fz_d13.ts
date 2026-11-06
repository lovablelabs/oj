import { createServerOnlyFn } from "@tanstack/react-start";
import { s } from "./m";
export function m() {
  function h() { return s }
  function k() { return h() }
  return createServerOnlyFn(/*<*/() => k()/*>*/)
}
