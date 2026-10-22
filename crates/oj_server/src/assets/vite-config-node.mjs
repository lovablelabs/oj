// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Loads the app's vite, and its vite.config, the way Node's Vite does. Vite branches on
// `process.versions.deno` in loadConfigFromFile: under Deno (oj's runtime) it
// treats every config as ESM and imports the bundled config from
// `<config>.timestamp-<ms>-<hash>.mjs` beside the source, not from
// node_modules/.vite-temp (vitejs/vite#18823, for Deno-native `npm:` imports).
// That dropped a short-lived file into the app root on every config load, and
// Start loads the config again in every client rebundle. oj only runs Node
// projects' configs, so the Deno branch is switched off for these calls.

import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";

let depth = 0;
let deno;

/** Runs `load` (a Vite call that reads the config file) as Node's Vite would.
 *  Overlapping calls share one window; the last to finish restores. */
export async function asNode(load) {
  if (depth++ === 0) {
    deno = Object.getOwnPropertyDescriptor(process.versions, "deno");
    if (deno) delete process.versions.deno;
  }
  try {
    return await load();
  } finally {
    if (--depth === 0 && deno) Object.defineProperty(process.versions, "deno", deno);
  }
}

/** The app's vite as Node's `import("vite")` from `root` finds it. */
export function appViteEntry(root) {
  return esmEntry(createRequire(join(root, "package.json")).resolve("vite"), "vite");
}

/** The file Node's `import(name)` loads for the package `resolved` (a path
 *  `require.resolve(name)` gave) lives in: its `exports["."]` under Node's
 *  import conditions, else `resolved`. `require.resolve` follows `require`,
 *  which for Vite 5 and 6 is the deprecated CJS proxy (`index.cjs`) exporting
 *  little more than `defineConfig`; Node's Vite runs `dist/node/index.js`. */
export function esmEntry(resolved, name) {
  let dir = dirname(resolved);
  for (let i = 0; i < 16; i++) {
    let pkg;
    try {
      pkg = JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));
    } catch {}
    if (pkg?.name === name) {
      const exp = pkg.exports;
      const dot =
        exp && typeof exp === "object" && !Array.isArray(exp) && Object.keys(exp).some((k) => k.startsWith("."))
          ? exp["."]
          : exp;
      const target = importTarget(dot);
      return target ? join(dir, target) : resolved;
    }
    const parent = dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  return resolved;
}

// An exports target under Node's import conditions, in the map's own key order.
function importTarget(target) {
  if (typeof target === "string") return target;
  if (Array.isArray(target)) {
    for (const t of target) {
      const r = importTarget(t);
      if (r) return r;
    }
    return null;
  }
  if (target && typeof target === "object") {
    for (const [key, value] of Object.entries(target)) {
      if (key === "node" || key === "import" || key === "default") {
        const r = importTarget(value);
        if (r) return r;
      }
    }
  }
  return null;
}
