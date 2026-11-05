import { createServerOnlyFn } from "@tanstack/react-start";
export function Comp() {
  const f = createServerOnlyFn(undefined);
  return f;
}
