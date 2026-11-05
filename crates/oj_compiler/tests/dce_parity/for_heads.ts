import { getRequest } from "@tanstack/react-start/server";
export function make(xs: number[]) {
  for (let i = 0, req = getRequest; i < 1; i++) { keep(/*<*/() => req()/*>*/); }
  for (const x of xs) use(x);
}
