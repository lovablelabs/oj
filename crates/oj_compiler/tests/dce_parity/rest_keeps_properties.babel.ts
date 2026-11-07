const {
  a,
  ...others
} = obj;
const {
  b
} = obj;
export const f = keep(undefined);
export const g = others + b;
