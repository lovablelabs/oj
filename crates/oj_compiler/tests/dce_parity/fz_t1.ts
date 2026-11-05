import { createServerOnlyFn } from "@tanstack/react-start";
export function make() {
  const {
    a,
    b,
  } = pick();
  const h = () => b;
  return createServerOnlyFn(/*<*/() => [a, h()]/*>*/);
}
