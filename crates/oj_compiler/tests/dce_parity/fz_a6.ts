import { createServerOnlyFn } from "@tanstack/react-start";
import { a, /* c1 */ b, // c2
  c, } from "./m";
export const f = createServerOnlyFn(/*<*/() => b + c/*>*/);
export const g = a;
