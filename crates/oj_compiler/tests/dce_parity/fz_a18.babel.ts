import { createServerOnlyFn } from "@tanstack/react-start";
export function m(p = (() => {
  return createServerOnlyFn(undefined);
})()) {
  return p;
}
export const n = () => {
  const b = g();
  return createServerOnlyFn(() => b);
};
