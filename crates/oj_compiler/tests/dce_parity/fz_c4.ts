import { createServerOnlyFn } from "@tanstack/react-start";
import { a } from "./m";
export function m() { class C { static s = a } return createServerOnlyFn(/*<*/() => C/*>*/) }
