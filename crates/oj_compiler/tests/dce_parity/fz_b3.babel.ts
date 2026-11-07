import { createServerOnlyFn } from "@tanstack/react-start";
function a() {
  return b() + createServerOnlyFn(undefined);
}
function b() {
  return a();
}
