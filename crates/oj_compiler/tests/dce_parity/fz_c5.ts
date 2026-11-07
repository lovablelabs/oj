import { createServerOnlyFn } from "@tanstack/react-start";
import { a, b } from "./m";
function h() { return a }
const { x, y } = obj(h);
export const f = createServerOnlyFn(/*<*/() => x/*>*/);
export const g = y;
