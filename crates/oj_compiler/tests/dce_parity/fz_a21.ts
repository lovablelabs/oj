import { createServerOnlyFn } from "@tanstack/react-start";
export function m() {
  for (var i = 0, h = g(); i < 1; i++) {}
  return createServerOnlyFn(/*<*/() => h/*>*/);
}
