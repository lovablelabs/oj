import { createServerOnlyFn } from "@tanstack/react-start";
import { a } from "./m";
class C { static s = a }
export const f = createServerOnlyFn(/*<*/() => C/*>*/);
