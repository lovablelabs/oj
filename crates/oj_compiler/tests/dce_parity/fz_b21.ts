import { createServerOnlyFn } from "@tanstack/react-start";
import { a } from "./m";
const fn = function h() { return a; };
export const f = createServerOnlyFn(/*<*/() => fn()/*>*/);
