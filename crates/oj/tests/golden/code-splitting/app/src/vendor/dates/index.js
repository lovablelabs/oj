export function today() {
  return new Date(0).toISOString().slice(0, 10);
}
