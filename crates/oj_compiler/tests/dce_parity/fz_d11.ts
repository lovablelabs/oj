import { createServerOnlyFn } from "@tanstack/react-start";
import * as S from "./m";
const { a, b } = S;
export const f = createServerOnlyFn(/*<*/() => a/*>*/);
export { b };
