import { createServerOnlyFn } from "@tanstack/react-start";
import { a, b } from "./m";
const x = a, y = b;
export { y as z };
export const f = createServerOnlyFn(/*<*/() => x/*>*/);
