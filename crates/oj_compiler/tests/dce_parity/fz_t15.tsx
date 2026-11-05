import { createServerOnlyFn } from "@tanstack/react-start";
import { getRequest } from "@tanstack/react-start/server";
export function Comp() {
  const Req = () => getRequest();
  const f = createServerOnlyFn(/*<*/() => <Req />/*>*/);
  return f;
}
