import { createServerOnlyFn } from "@tanstack/react-start";
import { getRequest } from "@tanstack/react-start/server";
export function make() {
  for (let i = 0, req = getRequest; i < 1; i++) { createServerOnlyFn(/*<*/() => req()/*>*/); }
}
