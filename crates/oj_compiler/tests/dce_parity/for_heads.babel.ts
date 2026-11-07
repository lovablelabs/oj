export function make(xs: number[]) {
  for (let i = 0; i < 1; i++) {
    keep(undefined);
  }
  for (const x of xs) use(x);
}
