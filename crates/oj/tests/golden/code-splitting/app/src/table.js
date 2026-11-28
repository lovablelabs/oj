export function table(rows) {
  return rows.map((r) => `| ${r} |`).join("\n");
}
