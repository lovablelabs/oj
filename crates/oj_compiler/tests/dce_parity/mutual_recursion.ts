import { getRequest } from "@tanstack/react-start/server";
const a = (n: number): unknown => (n ? b(n - 1) : null);
const b = (n: number): unknown => a(n) ?? getRequest();
export const f = keep(/*<*/() => a(3)/*>*/);
