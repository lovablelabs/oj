import { createServerOnlyFn } from "@tanstack/react-start";
import { getRequest } from "@tanstack/react-start/server";
if (globalThis.x) { var helper = getRequest; }
export const fn = createServerOnlyFn(/*<*/() => helper()/*>*/);
