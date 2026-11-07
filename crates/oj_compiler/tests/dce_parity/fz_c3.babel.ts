import { createServerOnlyFn } from "@tanstack/react-start";
import { register } from "./m";
register(() => 1);
const unused = register;
export const f = createServerOnlyFn(undefined);
