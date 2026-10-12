// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// babel-preset-solid for oj's Start bundles: mirrors vite-plugin-solid's
// transform hook (same preset, same per-environment options) so Solid JSX in
// the app AND in Solid dependencies (e.g. @tanstack/solid-router's dist
// .jsx) compiles to fine-grained DOM/SSR output instead of falling through
// to rolldown's React-automatic JSX. No solid-refresh HMR: Start edits
// rebuild the bundle and reload, like oj's React Start flow.
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

function appHasSolid(appRoot) {
  try {
    const pkg = JSON.parse(readFileSync(join(appRoot, "package.json"), "utf8"));
    const deps = { ...pkg.dependencies, ...pkg.devDependencies };
    return Object.keys(deps).some((d) => d === "solid-js" || d.startsWith("solid-js/"));
  } catch {
    return false;
  }
}

async function loadToolchain(appRoot) {
  const req = createRequire(pathToFileURL(join(appRoot, "package.json")).href);
  const babel = await import(pathToFileURL(req.resolve("@babel/core")).href);
  const presetMod = await import(pathToFileURL(req.resolve("babel-preset-solid")).href);
  return { babel, preset: presetMod.default ?? presetMod };
}

// `generate`: "dom" for the client bundle, "ssr" for the server bundle.
// Returns null when the app is not Solid or the toolchain (which ships with
// vite-plugin-solid) is unresolvable: callers keep the React-automatic path.
export async function solidBundlePlugin(appRoot, generate) {
  if (!appHasSolid(appRoot)) return null;
  let toolchain;
  try {
    toolchain = await loadToolchain(appRoot);
  } catch {
    return null;
  }
  const { babel, preset } = toolchain;
  const solidOptions = { generate, hydratable: true };
  return {
    name: "oj-solid-transform",
    async transform(source, id) {
      const clean = String(id).replace(/\?.*$/, "");
      if (!/\.[mc]?[tj]sx$/i.test(clean)) return null;
      const useTypescript = /\.[mc]?tsx$/i.test(clean);
      const { code, map } = await babel.transformAsync(source, {
        filename: clean,
        presets: [[preset, solidOptions]],
        plugins: [],
        parserOpts: { plugins: useTypescript ? ["jsx", "typescript"] : ["jsx"] },
        ast: false,
        sourceMaps: true,
        configFile: false,
        babelrc: false,
      });
      if (code == null) return null;
      return { code, map };
    },
  };
}

// Rolldown-side JSX for a Solid bundle: preserve (the plugin above owns the
// transform). Falls back to the caller's React-automatic options when the
// app is not Solid.
export function solidJsxOption(appRoot, fallback) {
  return appHasSolid(appRoot) ? "preserve" : fallback;
}

// Export conditions for a Solid client bundle: the `solid` condition first,
// as vite-plugin-solid's config hook orders it.
export function solidConditions(conditions, appRoot) {
  if (!appHasSolid(appRoot)) return conditions;
  if (conditions.includes("solid")) return conditions;
  return ["solid", ...conditions];
}

// Explicit export conditions for a Solid server bundle: `solid` first
// (selects transformable dist/source over precompiled dist/esm, whose
// top-level template() calls throw under the server build), then Vite's
// SSR order. React apps keep rolldown's defaults (spread of nothing).
export function solidServerConditions(appRoot, nodeEnv) {
  if (!appHasSolid(appRoot)) return {};
  return {
    conditionNames: [
      "solid",
      "module",
      "node",
      "import",
      nodeEnv === "production" ? "production" : "development",
      "default",
    ],
  };
}
