import { getRequest } from "@tanstack/react-start/server";
function hdr(n: string, d = 0): string | null { return d > 2 ? null : getRequest()?.headers.get(n) ?? hdr(n, d + 1); }
export const f = keep(/*<*/() => hdr("x")/*>*/);
