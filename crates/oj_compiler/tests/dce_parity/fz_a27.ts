import { createServerOnlyFn } from "@tanstack/react-start";
import x, { y } from "./m";
export const f = createServerOnlyFn(/*<*/() => x + y/*>*/);
