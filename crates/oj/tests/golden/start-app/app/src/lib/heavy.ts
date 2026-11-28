export function heavy(): string {
  return Array.from({ length: 3 }, (_, i) => `row ${i}`).join(", ");
}
