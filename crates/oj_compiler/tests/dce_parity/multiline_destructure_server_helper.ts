import { getRequest } from "@tanstack/react-start/server";
export function make() {
  const {
    a,
    b,
  } = pick();
  const h = () => b;
  const g = () => getRequest();
  return [keep(/*<*/() => [a, h(), g()]/*>*/)];
}
