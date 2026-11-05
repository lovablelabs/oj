import { createServerOnlyFn } from "@tanstack/react-start";
export function m() {
  const [x, ...ys] = obj;
  return createServerOnlyFn(/*<*/() => x + ys/*>*/);
}
