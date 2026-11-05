import { createServerOnlyFn } from "@tanstack/react-start";
import { A } from "./m";
export function make() {
  const schema = mk();
  type T = typeof schema;
  let v: T | A | undefined;
  return [v, createServerOnlyFn(/*<*/() => schema + new A()/*>*/)];
}
