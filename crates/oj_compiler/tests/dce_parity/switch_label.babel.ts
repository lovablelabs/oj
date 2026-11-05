export function f(k: number) {
  outer: switch (k) {
    case 1:
      {
        keep(undefined);
        break outer;
      }
  }
}
