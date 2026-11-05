export function make() {
  let cached;
  return keep(/*<*/() => { cached = 1; }/*>*/);
}
