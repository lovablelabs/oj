import { createServerOnlyFn } from "@tanstack/react-start";
import { a } from "./m";
const C = class { static s = a };
export const f = createServerOnlyFn(/*<*/() => C/*>*/);
