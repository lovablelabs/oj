import { createServerOnlyFn } from "@tanstack/react-start";
import { a } from "./m";
function h() { return a }
export default class { m() { return h() } }
export const f = createServerOnlyFn(/*<*/() => h()/*>*/);
