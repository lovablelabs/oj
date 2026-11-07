const { a, ...others } = obj;
const { b, ...gone } = obj;
export const f = keep(/*<*/() => a + gone.x/*>*/);
export const g = others + b;
