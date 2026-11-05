import { createServerOnlyFn } from "@tanstack/react-start";
import x, { y } from "./m";
export const f = createServerOnlyFn(/*<*/() => y/*>*/);
export const g = x;
