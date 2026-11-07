export function m() {
  return keep(/*<*/() => { const a = 3; return a }/*>*/) && (() => { const a = g(); return 1 })();
}
