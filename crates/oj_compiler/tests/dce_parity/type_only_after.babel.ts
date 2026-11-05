export function make() {
  const schema = mk();
  type T = typeof schema;
  let v: T | A | undefined;
  return [v, keep(undefined)];
}
