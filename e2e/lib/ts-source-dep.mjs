// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Synthesizes a dependency that ships raw TypeScript sources, physically under
// node_modules (a registry install, not a symlinked file: package, which the
// resolver follows out of node_modules): an exports map pointing straight at a
// .ts file, an enum (needs a real transform, not just type stripping), a
// type-only import, an extensionless relative .ts import, a NodeNext-style
// `./tone.js` import whose file on disk is tone.ts, a .tsx sibling that
// imports a plain .css, a CSS module and JSON (a component library's shape),
// and an exported stylesheet apps import bare (`ts-source-dep/styles.css`).

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
      exports: { "./badge": "./components/data-display/badge/index.ts", "./styles.css": "./styles.css" },
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
    `import "./icon.css";
import styles from "./icon.module.css";
import meta from "./icon.json";
export function BadgeIcon({ name }: { name: string }) {
  return (
    <span className={\`badge-icon \${styles.glyph ? "ts-dep-css-module" : ""}\`}>
      {\`ts-dep-icon:\${name}:ts-dep-json:\${meta.tag}\`}
    </span>
  );
}
`,
  );
  fs.writeFileSync(path.join(badge, "icon.css"), `.badge-icon { display: inline-flex; }\n`);
  fs.writeFileSync(path.join(badge, "icon.module.css"), `.glyph { color: red; }\n`);
  fs.writeFileSync(path.join(badge, "icon.json"), `{ "tag": "glyph" }\n`);
  fs.writeFileSync(path.join(dep, "styles.css"), `:root { --ts-dep: 1; }\n`);
  return dep;
}
