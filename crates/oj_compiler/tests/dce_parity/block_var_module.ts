import { getRequest } from "@tanstack/react-start/server";
if (globalThis.x) { var helper = getRequest; }
export const fn = keep(/*<*/() => helper()/*>*/);
