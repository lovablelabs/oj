import { createServerOnlyFn } from "@tanstack/react-start";
export namespace N {
  export const x = 1;
  export const f = createServerOnlyFn(/*<*/() => x/*>*/);
}
