export function make() {
  const schema = mk();
  type T = typeof schema;
  let v: T | undefined;
  return [v, keep(/*<*/() => schema/*>*/)];
}
