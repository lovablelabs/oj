export function make(obj: any) {
  const {} = obj;
  const [] = obj;
  return keep(/*<*/() => 1/*>*/);
}
