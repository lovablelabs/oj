import { createServerOnlyFn } from "@tanstack/react-start";
export class K {
  static {
    this.f = createServerOnlyFn(undefined);
  }
  x = (() => {
    const b = g();
    return createServerOnlyFn(() => b);
  })();
  get y() {
    const c = g();
    return createServerOnlyFn(() => c);
  }
}
