// SPDX-License-Identifier: MIT

// Imports a package the app depends on, directly or transitively: the route
// generator loads TanStack's router-generator through it, which a strict
// (pnpm) layout keeps out of the app root's own resolution.

import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { dirname, join } from "node:path";
import { readFileSync } from "node:fs";

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

export function makeResolver(root) {
  const appRequire = createRequire(pathToFileURL(root + "/package.json").href);
  const directDeps = depsOf(root + "/package.json");
  return function resolvePkg(spec, preferred = []) {
    try {
      return appRequire.resolve(spec);
    } catch {}
    // A transitive dependency is not resolvable from the app root under a
    // strict (pnpm) layout, so walk the dependency graph breadth-first,
    // re-anchoring resolution at each package we reach until `spec` resolves.
    const seen = new Set();
    let frontier = [];
    for (const a of [...preferred, ...directDeps]) {
      const pj = pkgJsonOf(appRequire, a);
      if (pj) frontier.push(pj);
    }
    for (let depth = 0; depth < 8 && frontier.length; depth++) {
      const next = [];
      for (const pj of frontier) {
        if (seen.has(pj)) continue;
        seen.add(pj);
        const req = createRequire(pathToFileURL(pj).href);
        try {
          return req.resolve(spec);
        } catch {}
        for (const d of depsOf(pj)) {
          const dpj = pkgJsonOf(req, d);
          if (dpj && !seen.has(dpj)) next.push(dpj);
        }
      }
      frontier = next;
    }
    throw new Error(`oj: cannot resolve '${spec}' from ${root}`);
  };
}

async function importResolved(p) {
  const m = await import(pathToFileURL(p).href);
  return m.default ?? m;
}

export async function importPkg(root, spec, preferred = []) {
  return importResolved(makeResolver(root)(spec, preferred));
}
