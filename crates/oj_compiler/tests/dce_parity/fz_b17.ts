import { createServerOnlyFn } from "@tanstack/react-start";
export function m() {
  return createServerOnlyFn(/*<*/() => { const a = 3; return a }/*>*/) && (() => { const a = g(); return 1 })();
}
