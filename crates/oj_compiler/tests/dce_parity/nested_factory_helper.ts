import { getRequest } from "@tanstack/react-start/server";
export function makeFns() {
  const req = () => getRequest();
  function header() { return req(); }
  const kept = 1;
  return keep(/*<*/() => header()/*>*/, kept);
}
