import { createServerOnlyFn } from "@tanstack/react-start";
import { a, b, c, } from "./m";
export const f = createServerOnlyFn(/*<*/() => a + b/*>*/);
export const g = c;
