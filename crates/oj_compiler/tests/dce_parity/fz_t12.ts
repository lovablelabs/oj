import { createServerOnlyFn } from "@tanstack/react-start";
import { getRequest } from "@tanstack/react-start/server";
export function make() {
  const {
    a,
    b,
  } = pick();
  const h = () => b;
  const g = () => getRequest();
  return [createServerOnlyFn(/*<*/() => [a, h(), g()]/*>*/)];
}
