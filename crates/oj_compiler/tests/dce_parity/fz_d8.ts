import { createServerOnlyFn } from "@tanstack/react-start";
import { s } from "./m";
const o = { h() { return s } };
export const f = createServerOnlyFn(/*<*/() => o.h()/*>*/);
