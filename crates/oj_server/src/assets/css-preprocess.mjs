// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Less / Stylus / SugarSS compiler for oj's in-process JS engine. The engine loads this
// module once and calls `compile` per request; there is no process protocol
// here.

import { createRequire } from "node:module";
import { existsSync, readFileSync, statSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";
import path from "node:path";

// A resolution failure for the preprocessor package itself carries this marker
// so the host can print the "is <package> installed?" hint.
function missingPackage(name, err) {
  return new Error(`OJ_MISSING_PACKAGE ${name}: ${(err && err.message) || err}`);
}

const load = async (base, spec) => {
  const req = createRequire(pathToFileURL(path.join(base, "package.json")).href);
  let resolved;
  try {
    resolved = req.resolve(spec);
  } catch (e) {
    throw missingPackage(spec, e);
  }
  return import(pathToFileURL(resolved).href).then((m) => m.default ?? m);
};

const isFile = (p) => {
  try {
    return statSync(p).isFile();
  } catch {
    return false;
  }
};

// A bare Less `@import` ("bootstrap/less/bootstrap", "~pkg/x", "pkg") resolved
// the way Vite's Less file manager does through its CSS resolver: node_modules
// walked up from the importing file (then the app root), package.json
// `less` / `style` / `main` fields for a bare package, `.less` / `.css`
// extensions and `index.less` for a subpath. Returns an absolute file or null.
function resolveBareLess(spec, dir, base) {
  if (!spec || spec.startsWith(".") || spec.startsWith("/") || path.isAbsolute(spec) || /^(?:https?:)?\/\//.test(spec))
    return null;
  if (spec.startsWith("~")) spec = spec.slice(1);
  const parts = spec.split("/");
  const pkgName = spec.startsWith("@") ? parts.slice(0, 2).join("/") : parts[0];
  const subpath = spec.slice(pkgName.length).replace(/^\//, "");
  const roots = [];
  for (let d = dir; ; d = path.dirname(d)) {
    roots.push(path.join(d, "node_modules"));
    if (path.dirname(d) === d) break;
  }
  roots.push(path.join(base, "node_modules"));
  for (const nm of roots) {
    const pkgDir = path.join(nm, pkgName);
    if (!existsSync(pkgDir)) continue;
    const candidates = [];
    if (subpath) {
      candidates.push(subpath, `${subpath}.less`, `${subpath}.css`, `${subpath}/index.less`);
    } else {
      let pkg = {};
      try {
        pkg = JSON.parse(readFileSync(path.join(pkgDir, "package.json"), "utf8"));
      } catch {}
      for (const field of ["less", "style", "main"]) {
        const v = pkg[field];
        if (typeof v === "string" && (field !== "main" || /\.(less|css)$/.test(v))) candidates.push(v);
      }
      candidates.push("index.less");
    }
    for (const c of candidates) {
      const p = path.join(pkgDir, c);
      if (isFile(p)) return p;
    }
  }
  return null;
}

// Vite's createViteLessPlugin: a FileManager that resolves bare specifiers
// through node_modules before Less falls back to its `paths` lookup.
function ojLessPlugin(lessLib, base) {
  const { FileManager } = lessLib;
  class OjFileManager extends FileManager {
    supports(filename) {
      return !/^(?:https?:)?\/\//.test(filename);
    }
    supportsSync() {
      return false;
    }
    async loadFile(filename, dir, opts, env) {
      const resolved = resolveBareLess(filename, dir || base, base);
      if (resolved) return { filename: resolved, contents: await readFile(resolved, "utf8") };
      return super.loadFile(filename, dir, opts, env);
    }
  }
  return {
    install(_, pluginManager) {
      pluginManager.addFileManager(new OjFileManager());
    },
    minVersion: [3, 0, 0],
  };
}

// `opts` is css.preprocessorOptions.less / .stylus, passed through like Vite
// does (javascriptEnabled, globalVars, modifyVars, paths, define, ...).
async function less(base, css, from, opts = {}) {
  const less = await load(base, "less");
  const { paths = [], plugins = [], ...rest } = opts;
  const out = await less.render(css, {
    ...rest,
    filename: from,
    // Vite defaults `paths: ['node_modules']` next to the resolver plugin.
    paths: [path.dirname(from), ...paths, path.join(base, "node_modules")],
    plugins: [ojLessPlugin(less, base), ...(Array.isArray(plugins) ? plugins : [])],
  });
  return out.css;
}

async function stylus(base, css, from, opts = {}) {
  const stylus = await load(base, "stylus");
  const { paths = [], define, imports, ...rest } = opts;
  return await new Promise((resolve, reject) => {
    const s = stylus(css)
      .set("filename", from)
      .set("paths", [path.dirname(from), ...paths]);
    for (const [k, v] of Object.entries(rest)) s.set(k, v);
    if (define && typeof define === "object") for (const [k, v] of Object.entries(define)) s.define(k, v);
    if (Array.isArray(imports)) for (const i of imports) s.import(i);
    s.render((err, out) => (err ? reject(err) : resolve(out)));
  });
}

// PostCSS from the app, else the one Vite itself depends on.
async function loadPostcss(base) {
  try {
    return await load(base, "postcss");
  } catch (e) {
    const req = createRequire(pathToFileURL(path.join(base, "package.json")).href);
    let vite;
    try {
      vite = req.resolve("vite/package.json");
    } catch {
      throw e;
    }
    const fromVite = createRequire(vite);
    return import(pathToFileURL(fromVite.resolve("postcss")).href).then((m) => m.default ?? m);
  }
}

// An `@import` specifier through `resolve.alias` (@rollup/plugin-alias:
// the whole specifier, or it followed by `/`), then from the root when it is
// root-absolute, else from the importing file.
function resolveSssImport(spec, from, opts) {
  for (const [find, replacement] of opts.alias ?? []) {
    if (spec === find || spec.startsWith(`${find}/`)) {
      const target = replacement + spec.slice(find.length);
      spec = target.startsWith(".") && opts.root ? path.join(opts.root, target) : target;
      break;
    }
  }
  if (path.isAbsolute(spec) && !existsSync(spec) && opts.root) spec = path.join(opts.root, spec);
  return path.isAbsolute(spec) ? spec : path.resolve(path.dirname(from), spec);
}

const cssUrlRE = /url\(\s*(['"]?)([^'")]+)\1\s*\)/g;

// Vite's sss: PostCSS with the app's `sugarss` as its parser (loadSss). An
// imported `.sss` is parsed the same way (postcss-import hands imported files
// the importer's parser), its relative `url()`s rebased to the importing
// sheet (Vite's url rewrite resolves each against its own file). The CSS
// printed goes on through the PostCSS config and the rest of the pipeline
// like any sheet; other `@import`s stay for it.
async function sugarss(base, css, from, opts = {}, seen = new Set()) {
  // sugarss exports its syntax (`{ parse, stringify }`), which PostCSS's
  // `parser` option takes as it is; older versions export `parse` itself.
  const sss = await load(base, "sugarss");
  const parse = typeof sss === "function" ? sss : sss.parse;
  const postcss = await loadPostcss(base);
  const root = parse(css, { from });
  const imports = [];
  root.walkAtRules("import", (rule) => {
    const spec = /^\s*(?:url\()?\s*(['"])(.+?)\1/.exec(rule.params)?.[2];
    if (spec && /\.sss$/.test(spec)) imports.push([rule, spec]);
  });
  for (const [rule, spec] of imports) {
    const file = resolveSssImport(spec, from, opts);
    if (seen.has(file)) {
      rule.remove();
      continue;
    }
    seen.add(file);
    const inner = await sugarss(base, await readFile(file, "utf8"), file, opts, seen);
    const rebased = inner.replace(cssUrlRE, (all, quote, url) => {
      if (!url.startsWith(".")) return all;
      let rel = path.relative(path.dirname(from), path.resolve(path.dirname(file), url)).replace(/\\/g, "/");
      if (!rel.startsWith(".")) rel = `./${rel}`;
      return `url(${quote}${rel}${quote})`;
    });
    rule.replaceWith(postcss.parse(rebased, { from: file }).nodes);
  }
  return root.toResult({ from }).css;
}

export async function compile(request) {
  const { base, css, from } = request;
  const opts = request.options && typeof request.options === "object" ? request.options : {};
  const ext = String(from || "")
    .split("?")[0]
    .split(".")
    .pop()
    .toLowerCase();
  if (ext === "less") return less(base, css, from, opts);
  if (ext === "styl" || ext === "stylus") return stylus(base, css, from, opts);
  if (ext === "sss") return sugarss(base, css, from, opts);
  return css;
}
