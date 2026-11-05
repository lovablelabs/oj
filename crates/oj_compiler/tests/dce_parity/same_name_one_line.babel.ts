export function make() {
  function k() {
    const a = side();
  }
  return [k, keep(undefined)];
}
