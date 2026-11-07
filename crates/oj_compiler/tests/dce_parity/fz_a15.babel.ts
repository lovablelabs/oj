import { createServerOnlyFn } from "@tanstack/react-start";
export function m(k) {
  switch (k) {
    case 1:
      return createServerOnlyFn(undefined);
    default:
      return 0;
  }
}
