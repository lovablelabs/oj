import { createServerOnlyFn } from "@tanstack/react-start";
import { a } from "./m";
enum E { X = a }
export const f = createServerOnlyFn(/*<*/() => E.X/*>*/);
