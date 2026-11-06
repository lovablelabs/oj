import { A } from "./m";
export function make() {
  const schema = mk();
  type T = typeof schema;
  let v: T | A | undefined;
  return [v, keep(/*<*/() => schema + new A()/*>*/)];
}
