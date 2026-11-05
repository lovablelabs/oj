import { createServerOnlyFn } from "@tanstack/react-start";
export function m() {
  const [,, [, z] = []] = obj;
  return [z, createServerOnlyFn(undefined)];
}
