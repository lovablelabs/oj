import { createServerOnlyFn } from "@tanstack/react-start";
import { a } from "./m";
export function m() {
  const { p: { q = a } = {}, r } = obj;
  return [r, createServerOnlyFn(/*<*/() => q/*>*/)];
}
