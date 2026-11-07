const a = (n: number): unknown => (n ? b(n - 1) : null);
const b = (n: number): unknown => a(n);
export const live = () => b(1);
export const f = keep(/*<*/() => a(3)/*>*/);
