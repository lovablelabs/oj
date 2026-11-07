import { createServerOnlyFn } from "@tanstack/react-start";
export class K {
  static { const a = g(); this.f = createServerOnlyFn(/*<*/() => a/*>*/); }
  x = (() => { const b = g(); return createServerOnlyFn(() => b) })();
  get y() { const c = g(); return createServerOnlyFn(() => c) }
}
