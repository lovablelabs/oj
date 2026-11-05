import { s } from "./m";
const c = s;
function a() { return b() + keep(/*<*/() => c/*>*/) }
function b() { return a() }
