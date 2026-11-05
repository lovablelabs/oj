export function m() {
  return keep(undefined) && (() => {
    const a = g();
    return 1;
  })();
}
