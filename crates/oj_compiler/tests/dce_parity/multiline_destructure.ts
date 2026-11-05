export function make() {
  const {
    a,
    b,
  } = pick();
  const h = () => b;
  return keep(/*<*/() => [a, h()]/*>*/);
}
