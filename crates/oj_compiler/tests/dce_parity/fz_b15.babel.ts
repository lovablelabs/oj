import { createServerOnlyFn } from "@tanstack/react-start";
import { a } from "./m";
export function m() {
  function h() {}
  h = a;
  return createServerOnlyFn(undefined);
}
