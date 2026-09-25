// SPDX-License-Identifier: MIT

import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { dirname, join } from "node:path";
import { existsSync, readFileSync, readdirSync } from "node:fs";

function depsOf(pkgJsonPath) {
  try {
    const pkg = JSON.parse(readFileSync(pkgJsonPath, "utf8"));
    return Object.keys({ ...pkg.dependencies, ...pkg.devDependencies, ...pkg.optionalDependencies });
  } catch { return []; }
}

// Locate a dependency's own package.json from `req`'s vantage point. The
// "<name>/package.json" subpath is the fast path; some packages don't expose it
// through their exports map, so fall back to resolving the entry and walking up.
function pkgJsonOf(req, name) {
  try { return req.resolve(name + "/package.json"); } catch {}
  try {
    let dir = dirname(req.resolve(name));
    for (let i = 0; i < 16; i++) {
      const cand = join(dir, "package.json");
      try { if (JSON.parse(readFileSync(cand, "utf8")).name === name) return cand; } catch {}
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
    try { return appRequire.resolve(spec); } catch {}
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
        try { return req.resolve(spec); } catch {}
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

// rolldown here is oj's own bundling tool, not the app's: the Start bundle
// scripts are written and tested against the version vendored next to the
// binary (OJ_VENDORED_ROLLDOWN, from oj's nix build), so that copy wins
// whenever it exists — the app's pin belongs to its vite, and old app pins
// carry bindings that cannot survive re-registration (napi-rs < 3.10). The
// app graph remains the fallback for oj builds that vendor nothing. This
// preference applies only at oj's own import sites: the app's module-graph
// resolution (makeResolver above, used by the plugin bridge) must keep
// resolving 'rolldown' and '@rolldown/*' to whatever the app's lockfile pins.
//
// A configured vendor is authoritative, never a hint: the bundle cache is
// keyed by the rolldown that built the bundle (like Vite's dep cache, keyed
// by the lockfile that pins its bundler), so a vendor that is set but broken
// must fail the build loudly — a silent fallback to the app's copy would
// cache an app-built bundle under the vendor's key. `OJ_VENDORED_ROLLDOWN=`
// (set but empty) is the explicit opt-out. rolldown must sit at exactly
// <vendor>/node_modules/rolldown: node's walk-up resolution would otherwise
// accept an ancestor's unrelated copy as "the vendor".
export function resolveOjRolldown(root, preferred = []) {
  const vendored = process.env.OJ_VENDORED_ROLLDOWN;
  if (vendored) {
    // Backstop for standalone-node runs: the authoritative validation (a
    // parseable package.json with a version) already ran in Rust
    // (oj_cache::start_bundle::vendored_rolldown) before this script spawned.
    if (!existsSync(join(vendored, "node_modules", "rolldown", "package.json"))) {
      throw new Error(
        `oj: OJ_VENDORED_ROLLDOWN (${vendored}) has no node_modules/rolldown; ` +
          "repair the vendor dir or set OJ_VENDORED_ROLLDOWN= (empty) to use the app's copy",
      );
    }
    return createRequire(pathToFileURL(join(vendored, "package.json")).href).resolve("rolldown");
  }
  try {
    return makeResolver(root)("rolldown", preferred);
  } catch {
    throw new Error(
      `oj: cannot resolve 'rolldown' from ${root}` +
        (vendored === ""
          ? "; OJ_VENDORED_ROLLDOWN= opted out of this build's vendored copy — unset it, or install 'rolldown' in the app"
          : "; this app's vite predates rolldown and this oj build vendors none — install 'rolldown' in the app or use an oj built with OJ_VENDORED_ROLLDOWN"),
    );
  }
}

export async function importOjRolldown(root, preferred = []) {
  const vendored = process.env.OJ_VENDORED_ROLLDOWN;
  try {
    return await importResolved(resolveOjRolldown(root, preferred));
  } catch (err) {
    // A vendor that resolves but cannot LOAD (a missing transitive dep in a
    // hand-curated bundle, a broken binding) must name the vendor too: the
    // raw loader error points nowhere near OJ_VENDORED_ROLLDOWN. A new Error
    // (never a mutation): the throw may be a primitive or a frozen object.
    const msg = err?.message ?? String(err);
    if (vendored && !`${msg}`.includes("OJ_VENDORED_ROLLDOWN")) {
      throw new Error(
        `oj: the vendored rolldown (OJ_VENDORED_ROLLDOWN=${vendored}) failed to load: ${msg}`,
        { cause: err },
      );
    }
    throw err;
  }
}

/// JSX transform options for rolldown / oxc-transform from `OJ_JSX` (the config's
/// `oxc.jsx` / `esbuild.jsx*`, serialized by oj). Defaults to the automatic React
/// runtime; a file's own `@jsx*` pragma comments still win inside oxc.
export function jsxTransformOptions(development) {
  let cfg = {};
  try { cfg = JSON.parse(process.env.OJ_JSX || "{}") || {}; } catch {}
  const classic = cfg.runtime === "classic";
  const out = { runtime: classic ? "classic" : "automatic" };
  if (development != null) out.development = development;
  if (!classic && cfg.importSource) out.importSource = cfg.importSource;
  if (classic && cfg.pragma) out.pragma = cfg.pragma;
  if (classic && cfg.pragmaFrag) out.pragmaFrag = cfg.pragmaFrag;
  return out;
}

// Vite's `envPrefix` (OJ_ENV_PREFIX from oj, a JSON list; `VITE_` by default).
export function envPrefixes(env = process.env) {
  try {
    const list = JSON.parse(env.OJ_ENV_PREFIX || "null");
    if (Array.isArray(list) && list.every((p) => typeof p === "string" && p)) return list;
  } catch {}
  return ["VITE_"];
}

export function viteEnvDefine({ ssr = false, mode = "development", env: envSource = process.env, base = "/", prefixes = envPrefixes() } = {}) {
  // Vite: DEV/PROD follow NODE_ENV (isProduction), MODE is the mode itself.
  const nodeEnv = envSource.NODE_ENV || (mode === "production" ? "production" : "development");
  const env = { MODE: mode, DEV: nodeEnv !== "production", PROD: nodeEnv === "production", SSR: !!ssr, BASE_URL: base };
  for (const [k, v] of Object.entries(envSource)) if (prefixes.some((p) => k.startsWith(p))) env[k] = v;
  return { "import.meta.env": JSON.stringify(env) };
}

// Vite's `environments.<name>.define` (OJ_DEFINE_CLIENT / OJ_DEFINE_SSR from
// oj): applied on top of the shared `define` for that environment's bundle.
export function environmentDefines(name, env = process.env) {
  const raw = name === "ssr" ? env.OJ_DEFINE_SSR : name === "client" ? env.OJ_DEFINE_CLIENT : null;
  try { return JSON.parse(raw || "{}") || {}; } catch { return {}; }
}

// Vite's `ssr.external` for the Start production server bundle (OJ_SSR_EXTERNALS
// from oj). oj's Start server build bundles its dependencies by default (dist/
// is self-contained, worker-ready), so `noExternal` has nothing left to do; an
// explicit `external` entry (or `external: true`) keeps that dependency a bare
// import of the bundle, resolved from node_modules at run time, the way Vite's
// ssrExternal leaves it out of the server build.
export function ssrExternalRule(appRoot, env = process.env) {
  let cfg = null;
  try { cfg = JSON.parse(env.OJ_SSR_EXTERNALS || "null"); } catch {}
  const names = new Set(Array.isArray(cfg?.external) ? cfg.external : []);
  const all = cfg?.externalAll === true;
  if (!names.size && !all) return () => false;
  const packageOf = (spec) => {
    const [first, second] = spec.split("/");
    return first.startsWith("@") ? (second ? `${first}/${second}` : null) : first || null;
  };
  const bare = (id) => !id.startsWith(".") && !id.startsWith("/") && !id.startsWith("\0") && !id.includes(":");
  return (id, _importer, isResolved) => {
    if (isResolved || !bare(id)) return false;
    const pkg = packageOf(id);
    if (!pkg) return false;
    if (names.has(pkg)) return true;
    // `external: true`: every installed dependency; an alias or a plugin virtual
    // that merely looks bare is not one.
    return all && existsSync(join(appRoot, "node_modules", pkg, "package.json"));
  };
}

const SRC_EXT = /\.(ts|tsx|js|jsx|mjs|cjs|mts|cts)$/;
export function emptyVirtualStub(appRoot, resolvedId) {
  const original = resolvedId.replace(/^\0/, "");
  const re = new RegExp(
    `import\\s+(?:type\\s+)?[^;{]*\\{([^}]*)\\}[^;]*?from\\s*["']${
      original.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")
    }["']`,
    "g",
  );
  const names = new Set();
  const scan = (dir) => {
    let entries;
    try {
      entries = readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const e of entries) {
      if (e.name === "node_modules" || e.name.startsWith(".")) continue;
      const p = join(dir, e.name);
      if (e.isDirectory()) {
        scan(p);
        continue;
      }
      if (!SRC_EXT.test(e.name)) continue;
      let src;
      try {
        src = readFileSync(p, "utf8");
      } catch {
        continue;
      }
      if (!src.includes(original)) continue;
      let m;
      while ((m = re.exec(src))) {
        for (let spec of m[1].split(",")) {
          spec = spec.trim();
          if (!spec || spec.startsWith("type ")) continue;
          const name = spec.split(/\s+as\s+/)[0].trim();
          if (name && name !== "default" && /^[A-Za-z_$][\w$]*$/.test(name)) names.add(name);
        }
      }
    }
  };
  scan(join(appRoot, "src"));
  let out = "export default undefined;\n";
  for (const n of names) out += `export const ${n} = undefined;\n`;
  return out;
}
