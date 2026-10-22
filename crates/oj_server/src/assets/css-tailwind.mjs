// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Tailwind / PostCSS compiler for oj's in-process JS engine. The engine loads
// this module once and calls `compile` per request; there is no process
// protocol here.

import { createRequire } from "node:module";
import { existsSync, readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { basename, dirname, join, resolve } from "node:path";

const processors = new Map();

// A resolution failure for the toolchain package itself carries this marker so
// the host can print the "is <package> installed?" hint.
function missingPackage(name, err) {
  return new Error(`OJ_MISSING_PACKAGE ${name}: ${(err && err.message) || err}`);
}

function depsOf(pkgJsonPath) {
  try {
    const pkg = JSON.parse(readFileSync(pkgJsonPath, "utf8"));
    return Object.keys({ ...pkg.dependencies, ...pkg.devDependencies, ...pkg.optionalDependencies });
  } catch {
    return [];
  }
}

// Locate a dependency's own package.json from `req`'s vantage point. The
// "<name>/package.json" subpath is the fast path; some packages don't expose it
// through their exports map, so fall back to resolving the entry and walking up.
function pkgJsonOf(req, name) {
  try {
    return req.resolve(name + "/package.json");
  } catch {}
  try {
    let dir = dirname(req.resolve(name));
    for (let i = 0; i < 16; i++) {
      const cand = join(dir, "package.json");
      try {
        if (JSON.parse(readFileSync(cand, "utf8")).name === name) return cand;
      } catch {}
      const parent = dirname(dir);
      if (parent === dir) break;
      dir = parent;
    }
  } catch {}
  return null;
}

// Resolve `spec` from the app root. Under a strict (pnpm) layout a transitive
// dependency is not visible from the root, so re-anchor resolution at each
// `anchors` package the app does depend on.
function resolvePkg(base, spec, anchors = []) {
  const req = createRequire(pathToFileURL(join(base, "package.json")).href);
  try {
    return req.resolve(spec);
  } catch {}
  for (const anchor of anchors) {
    const pj = pkgJsonOf(req, anchor);
    if (!pj) continue;
    try {
      return createRequire(pathToFileURL(pj).href).resolve(spec);
    } catch {}
  }
  return null;
}

async function importPkg(base, spec, anchors, hint) {
  const p = resolvePkg(base, spec, anchors);
  if (!p) throw missingPackage(hint, `cannot resolve '${spec}' from ${base}`);
  const m = await import(pathToFileURL(p).href);
  return m.default ?? m;
}

// The config file oj found the way postcss-load-config does (`postcssConfig` on
// the request: postcss.config.*, .postcssrc*, or a package.json with a
// `postcss` key, possibly in a parent directory up to the workspace root), read
// by its kind.
async function readPostcssConfig(base, cfgPath) {
  cfgPath ||= ["postcss.config.js", "postcss.config.cjs", "postcss.config.mjs"]
    .map((n) => `${base}/${n}`)
    .find((p) => existsSync(p));
  if (!cfgPath) return null;
  const name = basename(cfgPath);
  let config;
  if (name === "package.json") {
    config = JSON.parse(readFileSync(cfgPath, "utf8")).postcss;
  } else if (name === ".postcssrc" || name.endsWith(".json")) {
    config = JSON.parse(readFileSync(cfgPath, "utf8"));
  } else {
    const mod = await import(pathToFileURL(cfgPath).href);
    config = mod.default ?? mod;
  }
  if (typeof config === "function") {
    config = config({ env: process.env.NODE_ENV || "development", cwd: base, options: {} });
  }
  return config ? { config, dir: dirname(cfgPath) } : null;
}

// Resolve `postcss` and plugin packages from the config's own directory first
// (a workspace-root config installs them there), then from the app.
function resolver(dirs) {
  const reqs = dirs.map((d) => createRequire(pathToFileURL(d + "/package.json").href));
  return (spec) => {
    let err;
    for (const req of reqs) {
      try {
        return req.resolve(spec);
      } catch (e) {
        err = e;
      }
    }
    throw err;
  };
}

// Vite runs every PostCSS config on its own postcss (a dependency of vite,
// css.ts `import('postcss')`), which a strict (pnpm) layout keeps out of the
// app's resolution. The config's or the app's own copy serves an app run
// without vite.
function vitesPostcss(base) {
  try {
    const vitePkg = createRequire(pathToFileURL(join(base, "package.json")).href).resolve("vite/package.json");
    return createRequire(pathToFileURL(vitePkg).href).resolve("postcss");
  } catch {
    return null;
  }
}

function resolveOrNull(resolve, spec) {
  try {
    return resolve(spec);
  } catch {
    return null;
  }
}

async function loadPostcss(base, cfgPath) {
  const key = `${base}\0${cfgPath || ""}`;
  if (processors.has(key)) return processors.get(key);
  let processor = null;
  const found = await readPostcssConfig(base, cfgPath);
  if (found) {
    const resolve = resolver(found.dir === base ? [base] : [found.dir, base]);
    const postcssPath = vitesPostcss(base) ?? resolveOrNull(resolve, "postcss");
    // A config always runs: Vite has its own postcss. Without one here the
    // config would be skipped silently, so say what is missing instead.
    if (!postcssPath) throw missingPackage("postcss", `cannot resolve 'postcss' from ${found.dir}`);
    const postcss = (await import(pathToFileURL(postcssPath).href)).default;
    const raw = found.config.plugins ?? {};
    const plugins = [];
    if (Array.isArray(raw)) {
      for (const p of raw) if (p) plugins.push(p);
    } else {
      for (const [name, opts] of Object.entries(raw)) {
        if (opts === false) continue;
        const imported = await import(pathToFileURL(resolve(name)).href);
        const factory = imported.default ?? imported;
        plugins.push(typeof factory === "function" ? factory(opts ?? {}) : factory);
      }
    }
    processor = postcss(plugins);
  }
  processors.set(key, processor);
  return processor;
}

// Tailwind v4 with no PostCSS config, compiled as @tailwindcss/postcss does:
// `@import` / `@source` resolve from the sheet's own directory, the scan covers
// `source(...)` (the root, `**/*` when unset) plus every `@source`, and the
// scanner's files and globs are the sheet's dependencies. Both packages are
// transitive dependencies of whichever tailwind package the app installed.
async function v4Compile(base, css, from) {
  const anchors = ["@tailwindcss/vite", "@tailwindcss/postcss", "tailwindcss"];
  const tw = await importPkg(base, "@tailwindcss/node", anchors, "tailwindcss");
  const oxide = await importPkg(base, "@tailwindcss/oxide", anchors, "tailwindcss");
  const deps = [];
  const dirDeps = [];
  const compiler = await tw.compile(css, {
    base: from ? dirname(from) : base,
    from,
    onDependency: (path) => deps.push(path),
  });
  const scanner = new oxide.Scanner({ sources: v4Sources(base, compiler) });
  const candidates = scanner.scan();
  for (const file of scanner.files) {
    if (file !== from) deps.push(file);
  }
  for (const { base: dir, pattern } of scanner.globs) {
    if (pattern === "*" && dir === base) continue;
    if (pattern === "") deps.push(resolve(dir));
    else dirDeps.push({ dir: resolve(dir), glob: pattern });
  }
  return { css: compiler.build(candidates), deps, dirDeps };
}

// The scanner's sources for a compiled sheet. Before 4.1 the compiler named
// its `@source` entries `globs`, with no negation.
function v4Sources(base, compiler) {
  const root =
    compiler.root === "none"
      ? []
      : compiler.root == null
        ? [{ base, pattern: "**/*", negated: false }]
        : [{ ...compiler.root, negated: false }];
  const sources = compiler.sources ?? (compiler.globs ?? []).map((g) => ({ ...g, negated: false }));
  return root.concat(sources);
}

// The sheet's css plus what it read, as Vite's compileCSS collects PostCSS
// `dependency` / `dir-dependency` messages for the dev watcher.
export async function compile(request) {
  const { base, css, from } = request;
  const processor = await loadPostcss(base, request.postcssConfig || null);
  if (processor) {
    const result = await processor.process(css, { from, map: false });
    const deps = [];
    const dirDeps = [];
    for (const message of result.messages) {
      if (message.type === "dependency") {
        deps.push(message.file);
      } else if (message.type === "dir-dependency") {
        dirDeps.push({ dir: resolve(dirname(from), message.dir), glob: message.glob || "**" });
      }
    }
    return { css: result.css, deps, dirDeps };
  }
  return v4Compile(base, css, from);
}
