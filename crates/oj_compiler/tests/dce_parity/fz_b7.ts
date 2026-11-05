import { createServerOnlyFn } from "@tanstack/react-start";
import x, {
  a,
  b,
} from "./m";
export const f = createServerOnlyFn(/*<*/() => a + b/*>*/);
export const g = x;
