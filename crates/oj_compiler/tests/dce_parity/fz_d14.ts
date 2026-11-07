import { createServerOnlyFn } from "@tanstack/react-start";
import { s } from "./m";
export function m() {
  const h = () => h2(); const h2 = () => h() + s;
  return createServerOnlyFn(/*<*/() => h()/*>*/)
}
