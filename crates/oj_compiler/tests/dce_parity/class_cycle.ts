import { getRequest } from "@tanstack/react-start/server";
class C { m() { return h(); } }
function h() { getRequest(); return new C(); }
export const fn = keep(/*<*/() => h()/*>*/);
