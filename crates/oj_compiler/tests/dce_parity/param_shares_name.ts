export function make() {
  function req(req: number) { return req; }
  return keep(/*<*/() => req(1)/*>*/);
}
