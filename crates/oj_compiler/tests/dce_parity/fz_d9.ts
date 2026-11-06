import { createServerOnlyFn } from "@tanstack/react-start";
import { s } from "./m";
export function m() {
  const { a, b: [c, d = s] } = obj;
  return [a, createServerOnlyFn(/*<*/() => c + d/*>*/)];
}
