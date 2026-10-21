// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Vite's own glob libraries, picomatch and tinyglobby (runtime dependencies
// of Vite), loaded from the app's `vite` so oj expands and matches globs with
// the code Vite runs: import.meta.glob expansion and its added-file matchers,
// and plugin hook `filter` ids. An app without them (a plugin config run with
// no Vite installed) matches filter ids with the picomatch release oj ships
// (./picomatch.mjs); only glob expansion needs tinyglobby, and Start apps
// always have Vite.

import { createRequire } from "node:module";
import { join, posix } from "node:path";
import { pathToFileURL } from "node:url";

let loaded = null;

// Loads both libraries once per process: from the app's `vite` first, then the
// app root, then (picomatch only) oj's copy.
export async function loadViteGlobs(root) {
  if (loaded) return loaded;
  const requirers = [];
  const app = createRequire(join(root, "package.json"));
  try {
    requirers.push(createRequire(app.resolve("vite")));
  } catch {}
  requirers.push(app);
  const load = async (name) => {
    for (const req of requirers) {
      let file;
      try {
        file = req.resolve(name);
      } catch {
        continue;
      }
      return import(pathToFileURL(file).href);
    }
    return null;
  };
  const [pm, tg] = await Promise.all([load("picomatch"), load("tinyglobby")]);
  const mod = pm ?? (await import("./picomatch.mjs"));
  loaded = { picomatch: mod.default ?? mod, tinyglobby: tg, root };
  return loaded;
}

export function viteGlobs() {
  if (!loaded) throw new Error("oj: Vite's glob libraries are not loaded (loadViteGlobs)");
  return loaded;
}

function tinyglobby() {
  const { tinyglobby, root } = viteGlobs();
  if (!tinyglobby) throw new Error(`oj: tinyglobby (a dependency of vite) is not resolvable from ${root}`);
  return tinyglobby;
}

// Vite's pluginFilter `getMatcherString` + `pm(glob, { dot: true })`, as a
// RegExp (rolldown takes hook filters as RegExps): a string id filter is a
// glob joined to `cwd` unless it starts with `**` or is absolute.
const slash = (p) => p.replace(/\\/g, "/");
const idFilterCache = new Map();
export function idFilterRegExp(pattern, cwd) {
  const glob =
    pattern.startsWith("**") || posix.isAbsolute(slash(pattern)) ? slash(pattern) : slash(join(cwd, pattern));
  let re = idFilterCache.get(glob);
  if (!re) {
    re = viteGlobs().picomatch.makeRe(glob, { dot: true });
    idFilterCache.set(glob, re);
  }
  return re;
}

// Vite's importMetaGlob getCommonBase: the deepest directory every affirmed
// pattern's scan base shares, where the walk starts.
export function getCommonBase(globsResolved) {
  const { picomatch } = viteGlobs();
  const bases = globsResolved
    .filter((g) => g[0] !== "!")
    .map((glob) => {
      let { base } = picomatch.scan(glob);
      if (posix.basename(base).includes(".")) base = posix.dirname(base);
      return base;
    });
  if (!bases.length) return null;
  let commonAncestor = "";
  const dirS = bases[0].split("/");
  for (let i = 0; i < dirS.length; i++) {
    const candidate = dirS.slice(0, i + 1).join("/");
    if (bases.every((base) => base === candidate || base.startsWith(`${candidate}/`))) commonAncestor = candidate;
    else break;
  }
  if (!commonAncestor) commonAncestor = "/";
  return commonAncestor;
}

// Vite's transformGlobImport file list for one import.meta.glob call.
export function globFiles(globsResolved, { exhaustive = false, caseSensitive = true, cwd }) {
  return tinyglobby()
    .globSync(globsResolved, {
      absolute: true,
      cwd: getCommonBase(globsResolved) ?? cwd,
      dot: exhaustive,
      expandDirectories: false,
      caseSensitiveMatch: caseSensitive,
      ignore: exhaustive ? [] : ["**/node_modules/**"],
      extglob: false,
    })
    .sort();
}

// Vite's globSafePath: a path prefix of a glob, with glob syntax escaped.
export function globSafePath(path) {
  return tinyglobby().escapePath(slash(path));
}
