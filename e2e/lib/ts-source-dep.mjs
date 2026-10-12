// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Synthesizes a dependency that ships raw TypeScript sources, physically under
// node_modules (a registry install, not a symlinked file: package, which the
// resolver follows out of node_modules): an exports map pointing straight at a
// .ts file, an enum (needs a real transform, not just type stripping), a
// type-only import, an extensionless relative .ts import, a NodeNext-style
// `./tone.js` import whose file on disk is tone.ts, and a .tsx sibling.

import fs from "node:fs";
import path from "node:path";

export function writeTsSourceDep(nodeModulesDir) {
  const dep = path.join(nodeModulesDir, "ts-source-dep");
  fs.rmSync(dep, { recursive: true, force: true });
  const badge = path.join(dep, "components", "data-display", "badge");
  fs.mkdirSync(badge, { recursive: true });
  fs.writeFileSync(
    path.join(dep, "package.json"),
    JSON.stringify({
      name: "ts-source-dep",
      version: "1.0.0",
      type: "module",
      exports: { "./badge": "./components/data-display/badge/index.ts" },
    }),
  );
  fs.writeFileSync(
    path.join(badge, "index.ts"),
    `import { badgeClass, type Variant } from "./variants";
import { toneLabel } from "./tone.js";
export { BadgeIcon } from "./icon";
export enum Tone {
  Neutral = "neutral",
  Bold = "bold",
}
export interface BadgeProps {
  label: string;
  variant?: Variant;
}
export function describeBadge(p: BadgeProps): string {
  return \`\${badgeClass(p.variant ?? "default")}:\${Tone.Bold}:\${toneLabel}:\${p.label}\`;
}
`,
  );
  fs.writeFileSync(
    path.join(badge, "tone.ts"),
    `export const toneLabel: string = "nodenext";
`,
  );
  fs.writeFileSync(
    path.join(badge, "variants.ts"),
    `export type Variant = "default" | "outline";
export const badgeClass = (v: Variant): string => \`badge-\${v}\`;
`,
  );
  fs.writeFileSync(
    path.join(badge, "icon.tsx"),
    `export function BadgeIcon({ name }: { name: string }) {
  return <span className="badge-icon">{\`ts-dep-icon:\${name}\`}</span>;
}
`,
  );
  return dep;
}
