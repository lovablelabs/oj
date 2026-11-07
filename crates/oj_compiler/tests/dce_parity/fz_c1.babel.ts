import { createServerOnlyFn } from "@tanstack/react-start";
import { s } from "./m";
const c = s;
function a() {
  return b() + c;
}
function b() {
  return a();
}
export const f = createServerOnlyFn(undefined);
