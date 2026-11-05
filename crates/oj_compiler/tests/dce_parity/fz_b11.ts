import { createServerOnlyFn } from "@tanstack/react-start";
export function m() {
  const { [k()]: v, w } = obj;
  const k = () => 1;
  return [w, createServerOnlyFn(/*<*/() => v/*>*/)];
}
