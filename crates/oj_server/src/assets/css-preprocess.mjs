// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { createRequire } from "node:module";
import { existsSync, fstatSync, readFileSync, statSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";
import path from "node:path";
import readline from "node:readline";

const load = (base, spec) => {
  const req = createRequire(path.join(base, "package.json"));
  return import(pathToFileURL(req.resolve(spec)).href).then((m) => m.default ?? m);
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
  if (!spec || spec.startsWith(".") || spec.startsWith("/") || path.isAbsolute(spec) || /^(?:https?:)?\/\//.test(spec)) return null;
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
    const s = stylus(css).set("filename", from).set("paths", [path.dirname(from), ...paths]);
    for (const [k, v] of Object.entries(rest)) s.set(k, v);
    if (define && typeof define === "object") for (const [k, v] of Object.entries(define)) s.define(k, v);
    if (Array.isArray(imports)) for (const i of imports) s.import(i);
    s.render((err, out) => (err ? reject(err) : resolve(out)));
  });
}

const rl = readline.createInterface({ input: process.stdin });
let inflight = 0;
let stdinClosed = false;

const maybeExit = () => {
  if (!stdinClosed || inflight !== 0) return;

  process.stdout.end((err) => {
    process.exit(err ? 1 : 0)
  })
};

try {
  if (fstatSync(0, { bigint: true }).isFIFO()) rl.once("close", () => { stdinClosed = true; maybeExit(); });
} catch {}
rl.on("line", async (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  const { id, base, css, from } = msg;
  const opts = msg.options && typeof msg.options === "object" ? msg.options : {};
  const ext = String(from || "").split("?")[0].split(".").pop().toLowerCase();
  inflight += 1;
  try {
    let out = css;
    if (ext === "less") out = await less(base, css, from, opts);
    else if (ext === "styl" || ext === "stylus") out = await stylus(base, css, from, opts);
    process.stdout.write(JSON.stringify({ id, css: out }) + "\n");
  } catch (e) {
    process.stdout.write(JSON.stringify({ id, error: String((e && e.message) || e) }) + "\n");
  } finally {
    inflight -= 1;
    maybeExit();
  }
});
