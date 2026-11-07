import { createServerOnlyFn } from "@tanstack/react-start";
import { default as x, "a-b" as y, z } from "./m";
export const f = createServerOnlyFn(/*<*/() => x + y/*>*/);
export const g = z;
