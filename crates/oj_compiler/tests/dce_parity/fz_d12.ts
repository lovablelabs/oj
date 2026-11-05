import { createServerOnlyFn } from "@tanstack/react-start";
import { s } from "./m";
export function m() {
  try { var h = s } catch { }
  return createServerOnlyFn(/*<*/() => h/*>*/)
}
