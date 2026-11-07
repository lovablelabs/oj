import { a } from "./m";
class C { static s = a }
export const f = keep(/*<*/() => C/*>*/);
