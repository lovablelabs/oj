import { createServerOnlyFn } from "@tanstack/react-start";
import { a } from "./m";
const x = a;
export default x;
export const f = createServerOnlyFn(/*<*/() => x/*>*/);
