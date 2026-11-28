export function table(rows: string[]): string {
  return rows.map((r) => `| ${r} |`).join("\n");
}
