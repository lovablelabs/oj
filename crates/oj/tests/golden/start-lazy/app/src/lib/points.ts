export function points(n: number) {
  return Array.from({ length: n }, (_, i) => (i * 37) % 100);
}
