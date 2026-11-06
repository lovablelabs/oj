import { createServerOnlyFn } from "@tanstack/react-start";
export function m() {
  const [ , x, [y, z] = [] ] = obj;
  return [z, createServerOnlyFn(/*<*/() => x + y/*>*/)];
}
