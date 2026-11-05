import { createServerOnlyFn } from "@tanstack/react-start";
import { s } from "./m";
const c = s;
function a() { return b() + createServerOnlyFn(/*<*/() => c/*>*/) }
function b() { return a() }
