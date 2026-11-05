import { createServerOnlyFn } from "@tanstack/react-start";
export function C() {
  return <div>{createServerOnlyFn(undefined)}</div>;
}
