import { getRequest } from "@tanstack/react-start/server";
export function Page() {
  const Req = () => <p>{String(getRequest())}</p>;
  return keep(/*<*/() => <Req />/*>*/);
}
