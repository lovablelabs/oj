import { createServerOnlyFn } from "@tanstack/react-start";
export function m() {
  const a = g(); const a2 = 1; return createServerOnlyFn(/*<*/() => { const a = 3; return a }/*>*/) && a2;
}
