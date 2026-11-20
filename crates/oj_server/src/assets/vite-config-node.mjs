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

import { existsSync, readFileSync, realpathSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { pathToFileURL } from "node:url";

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

/** Vite's basePluginContextMeta as the app's `vite` module has it: `viteVersion`
 *  from Vite 7, `rolldownVersion` when exported. Without a `vite`, or with one
 *  that states no `version` (every Vite exports it), Vite 8's whole meta: a
 *  module of unknown version is not read for the other keys either. */
export function pluginContextMeta(vite) {
  if (typeof vite?.version !== "string") {
    return { viteVersion: "8.0.0", rollupVersion: "4.0.0", rolldownVersion: "1.0.0" };
  }
  const meta = parseInt(vite.version, 10) < 7 ? {} : { viteVersion: vite.version };
  meta.rollupVersion = typeof vite.rollupVersion === "string" ? vite.rollupVersion : "4.0.0";
  if (typeof vite.rolldownVersion === "string") meta.rolldownVersion = vite.rolldownVersion;
  return meta;
}

// The package `name` as Node resolves it from `fromDir` (its real directory).
function packageDir(fromDir, name) {
  for (let dir = fromDir; ; dir = dirname(dir)) {
    const candidate = join(dir, "node_modules", name);
    if (existsSync(join(candidate, "package.json"))) return realpathSync(candidate);
    if (dirname(dir) === dir) return null;
  }
}

// The @tanstack/start-plugin-core the app's Start framework package runs.
function startPluginCoreDir(root) {
  for (const framework of ["@tanstack/react-start", "@tanstack/solid-start", "@tanstack/vue-start"]) {
    const dir = packageDir(root, framework);
    if (dir) return packageDir(dir, "@tanstack/start-plugin-core");
  }
  return null;
}

// A pattern of Start's import protection as Rust reads it: a string as
// `{ text }`, a RegExp as `{ source, flags }`, anything else as `{ other }`.
function importProtectionPattern(p) {
  if (typeof p === "string") return { text: p };
  if (p instanceof RegExp) return { source: p.source, flags: p.flags };
  return { other: true };
}
const importProtectionPatterns = (list) => (Array.isArray(list) ? list.map(importProtectionPattern) : null);
const importProtectionRules = (rules) => ({
  specifiers: importProtectionPatterns(rules?.specifiers),
  files: importProtectionPatterns(rules?.files),
  excludeFiles: importProtectionPatterns(rules?.excludeFiles),
});

/** Records the options `tanstackStart()` is validated with while the app's
 *  config resolves: Start parses them once, through the options schema its
 *  plugin core exports, so the schema's own run is where they pass. Returns
 *  `read`, the import protection options and `srcDirectory` in the shape
 *  Rust reads (null when Start parsed nothing, or its schema is not the one
 *  this reads), and `restore`. */
export async function recordStartImportProtection(root) {
  const none = { read: () => null, restore() {} };
  const dir = startPluginCoreDir(root);
  if (!dir) return none;
  let internals;
  let version;
  try {
    version = JSON.parse(readFileSync(join(dir, "package.json"), "utf8")).version;
    const schema = await import(pathToFileURL(join(dir, "dist/esm/schema.js")).href);
    internals = schema.tanstackStartOptionsObjectSchema?._zod;
  } catch {
    return none;
  }
  const run = internals?.run;
  if (typeof run !== "function") return none;
  let seen = null;
  try {
    internals.run = function (payload, ...rest) {
      const options = payload?.value;
      if (options && typeof options === "object") seen = options;
      return run.call(this, payload, ...rest);
    };
  } catch {
    return none;
  }
  return {
    read() {
      if (!seen) return null;
      const ip = seen.importProtection ?? {};
      const behavior = typeof ip.behavior === "string" ? ip.behavior : (ip.behavior?.dev ?? "mock");
      return {
        // The plugin core these options are for: Rust stands in for the
        // versions whose import protection it was written against.
        version: typeof version === "string" ? version : null,
        srcDirectory: typeof seen.srcDirectory === "string" ? seen.srcDirectory : "src",
        enabled: ip.enabled !== false,
        devBehavior: behavior,
        mockAccess: ip.mockAccess ?? "error",
        onViolation: typeof ip.onViolation === "function",
        include: importProtectionPatterns(ip.include) ?? [],
        exclude: importProtectionPatterns(ip.exclude) ?? [],
        ignoreImporters: importProtectionPatterns(ip.ignoreImporters) ?? [],
        client: importProtectionRules(ip.client),
        server: importProtectionRules(ip.server),
        maxTraceDepth: typeof ip.maxTraceDepth === "number" ? ip.maxTraceDepth : null,
      };
    },
    restore() {
      internals.run = run;
    },
  };
}

/** The router generator's `Generator` class Start's router plugin runs,
 *  resolved the way Start's own packages resolve it (so it is the same module
 *  instance), or null for an app without Start. */
export async function startRouterGenerator(root) {
  let dir = startPluginCoreDir(root);
  for (const name of ["@tanstack/router-plugin", "@tanstack/router-generator"]) {
    dir = dir && packageDir(dir, name);
  }
  if (!dir) return null;
  try {
    const entry = esmEntry(join(dir, "package.json"), "@tanstack/router-generator");
    return (await import(pathToFileURL(entry).href)).Generator ?? null;
  } catch {
    return null;
  }
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
