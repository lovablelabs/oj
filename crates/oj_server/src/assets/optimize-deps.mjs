// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { mkdirSync, rmSync, readFileSync, writeFileSync, renameSync, existsSync, realpathSync, globSync } from "node:fs";
import path from "node:path";
import builtinModules from "node:module";

// Pre-bundle inputs, assigned by `optimize()` below. The optimizer engine is
// short-lived (one isolate per run), so per-run state lives safely at module
// scope; a direct import for unit tests runs no side effects.
let root;
let outDir;
let entries;
let include;
let exclude;
let dedupe;
let alias;
let autoDiscover;
let resolveSettings;
// optimizeDeps.needsInterop: Vite's needsInterop() returns true for these before
// looking at the bundle's export shape, so the metadata must say so too.
let NEEDS_INTEROP;
let resolveConditions;
let esbuildOptions;
let DEDUPE;
let req;
let excludeSet;
let DEDUPE_PKGS;
let aliasEntries;
let esbuild;

// esbuild activates import/require/default itself and rejects them in `conditions`.
const ESBUILD_IMPLICIT_CONDITIONS = new Set(["import", "require", "default"]);

const ESBUILD_OPTION_KEYS = new Set([
  "define", "target", "supported", "loader", "jsx", "jsxDev", "jsxSideEffects",
  "jsxFactory", "jsxFragment", "jsxImportSource", "mainFields", "conditions",
  "resolveExtensions", "preserveSymlinks", "keepNames", "minify", "minifyWhitespace",
  "minifyIdentifiers", "minifySyntax", "treeShaking", "platform", "external", "banner",
  "footer", "inject", "alias", "drop", "pure", "charset", "legalComments", "tsconfig",
  "tsconfigRaw", "ignoreAnnotations",
]);

function detectEntries() {
  const html = path.join(root, "index.html");
  if (!existsSync(html)) return [];
  const src = readFileSync(html, "utf8");
  const found = [];
  for (const tag of src.match(/<script\b[^>]*>/gi) || []) {
    if (!/type\s*=\s*["']module["']/i.test(tag)) continue;
    const m = tag.match(/\bsrc\s*=\s*["']([^"']+)["']/i);
    if (!m || /^https?:/i.test(m[1])) continue;
    const abs = path.join(root, m[1].replace(/^\//, ""));
    if (existsSync(abs)) found.push(abs);
  }
  return found;
}

// Vite's expandGlobIds (optimizer/resolve.ts): an include entry with a glob in
// its subpath (`some-pkg/*`, `@scope/pkg/dist/**/*.js`) expands to the package
// plus every subpath the glob matches: the package's `exports` keys when it
// has an exports map (subpath patterns resolved through their first target),
// otherwise the files under the package directory.
const isDynamicPattern = (id) => /[*?[\]{}]/.test(id);
const npmPackageName = (id) => {
  const parts = id.split("/");
  if (id.startsWith("@")) return parts.length >= 2 ? `${parts[0]}/${parts[1]}` : null;
  return parts[0] || null;
};
function findPackageDir(pkgName) {
  let dir = root;
  for (;;) {
    const candidate = path.join(dir, "node_modules", pkgName);
    if (existsSync(path.join(candidate, "package.json"))) return candidate;
    const parent = path.dirname(dir);
    if (parent === dir) return null;
    dir = parent;
  }
}
const firstExportString = (v) => {
  if (typeof v === "string") return v;
  if (Array.isArray(v)) return firstExportString(v[0]);
  if (v && typeof v === "object") for (const k in v) return firstExportString(v[k]);
  return undefined;
};
const globFiles = (pattern, cwd) => {
  try {
    return globSync(pattern, { cwd, exclude: (name) => name === "node_modules" }).map((p) => p.split(path.sep).join("/"));
  } catch {
    return [];
  }
};
const matchesGlob = (subject, pattern) => {
  try {
    return path.posix.matchesGlob(subject, pattern.replace(/^\.\//, ""));
  } catch {
    return false;
  }
};
function expandGlobIds(id) {
  const pkgName = npmPackageName(id);
  if (!pkgName) return [];
  const pkgDir = findPackageDir(pkgName);
  if (!pkgDir) return [];
  let pkgJson;
  try {
    pkgJson = JSON.parse(readFileSync(path.join(pkgDir, "package.json"), "utf8"));
  } catch {
    return [];
  }
  const pattern = "." + id.slice(pkgName.length);
  const exports = pkgJson.exports;
  if (exports) {
    if (typeof exports === "string" || Array.isArray(exports)) return [pkgName];
    const possible = [];
    for (const key of Object.keys(exports)) {
      if (key[0] !== ".") continue;
      if (key.includes("*")) {
        const value = firstExportString(exports[key]);
        if (!value) continue;
        const valueRe = new RegExp(value.split("*").map((s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("(.*)"));
        for (let file of globFiles(value.replace(/\*/g, "**/*"), pkgDir)) {
          if (value.startsWith("./")) file = "./" + file;
          const m = valueRe.exec(file);
          if (!m) continue;
          let same = true;
          for (let i = 2; i < m.length; i++) if (m[i] !== m[i - 1]) { same = false; break; }
          if (same) possible.push(key.replace("*", m[1]).slice(2));
        }
      } else if (exports[key] != null) {
        possible.push(key.slice(2));
      }
    }
    return [pkgName, ...possible.filter((p) => matchesGlob(p, pattern)).map((p) => path.posix.join(pkgName, p))];
  }
  return [pkgName, ...globFiles(pattern, pkgDir).map((m) => path.posix.join(pkgName, m))];
}
let includeIds;
let entryList;

// resolve.dedupe: a bare import of a deduped package resolves from the project
// root wherever the importer sits (Vite resolve.ts: dedupe -> basedir = root),
// so the pre-bundle holds the one copy the dev server also serves, not a copy
// nested under some dependency.
const dedupeFromRoot = {
  name: "oj-dedupe-from-root",
  setup(build) {
    if (!DEDUPE_PKGS.size) return;
    build.onResolve({ filter: /^[^./]/ }, async (args) => {
      if (args.pluginData?.ojDedupe || args.kind === "entry-point" || path.isAbsolute(args.path)) return null;
      if (!args.resolveDir || path.resolve(args.resolveDir) === path.resolve(root)) return null;
      if (!DEDUPE_PKGS.has(npmPackageName(args.path))) return null;
      const r = await build.resolve(args.path, {
        kind: args.kind,
        importer: args.importer,
        resolveDir: root,
        pluginData: { ojDedupe: true },
      });
      return r.errors.length ? null : { path: r.path, external: r.external, namespace: r.namespace };
    });
  },
};
// Vite 8 apps use rolldown for optimizeDeps and don't depend on esbuild directly,
// but esbuild is still a Vite transitive dep -- resolve it from the app's Vite when
// the app itself doesn't expose it, so the dep pre-bundler still runs.
function resolveEsbuild() {
  try {
    return req.resolve("esbuild");
  } catch {}
  // A Vite app rarely depends on esbuild directly but always has it transitively
  // through Vite, so resolve it from Vite's own directory. When neither is
  // present there is nothing to pre-bundle with: fail with a clear message (oj
  // then serves deps natively) instead of a misleading "cannot find
  // vite/package.json" from an app that simply has no Vite.
  try {
    return createRequire(req.resolve("vite/package.json")).resolve("esbuild");
  } catch {
    throw new Error("esbuild not found (neither directly nor via vite); dep pre-bundling skipped");
  }
}
const NODE_BUILTINS = new Set([...builtinModules.builtinModules, ...builtinModules.builtinModules.map((m) => "node:" + m)]);
const isBare = (id) => id && !id.startsWith(".") && !id.startsWith("/") && !id.startsWith("\0") && !NODE_BUILTINS.has(id);

// Strip // and /* */ comments from JSONC, but only outside string literals. A
// regex stripper is wrong here: a path pattern like "./src/modules/*" contains
// `/*`, which a naive block-comment regex treats as a comment start and eats
// through the next `*/`, corrupting the file (this silently broke tsconfig
// `paths` loading, so aliased imports were never resolved during the dep scan).
function stripJsonc(s) {
  let out = "";
  let inStr = false;
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    const c2 = s[i + 1];
    if (inStr) {
      out += c;
      if (c === "\\") {
        out += c2 ?? "";
        i++;
      } else if (c === '"') {
        inStr = false;
      }
      continue;
    }
    if (c === '"') {
      inStr = true;
      out += c;
      continue;
    }
    if (c === "/" && c2 === "/") {
      while (i < s.length && s[i] !== "\n") i++;
      out += "\n";
      continue;
    }
    if (c === "/" && c2 === "*") {
      i += 2;
      while (i < s.length && !(s[i] === "*" && s[i + 1] === "/")) i++;
      i++;
      continue;
    }
    out += c;
  }
  return out;
}

// Read a tsconfig with its `extends` chain (relative, or a package such as
// `@tsconfig/node20/tsconfig.json`) merged the way tsc does: `paths` and
// `baseUrl` from the nearest file win, `baseUrl` resolves relative to the file
// that declares it. This mirrors the Rust resolver's oxc tsconfig handling
// closely enough that the pre-bundle sees the same aliases as the dev server.
function readTsconfigChain(file, seen = new Set()) {
  if (seen.has(file) || !existsSync(file)) return null;
  seen.add(file);
  let json;
  try {
    json = JSON.parse(stripJsonc(readFileSync(file, "utf8")));
  } catch {
    return null;
  }
  const own = json.compilerOptions || {};
  let merged = { paths: undefined, pathsBase: undefined };
  const parents = Array.isArray(json.extends) ? json.extends : json.extends ? [json.extends] : [];
  for (const ext of parents) {
    let parentFile = null;
    if (ext.startsWith(".") || path.isAbsolute(ext)) {
      parentFile = path.resolve(path.dirname(file), ext);
      if (!existsSync(parentFile) && existsSync(parentFile + ".json")) parentFile += ".json";
    } else {
      try { parentFile = req.resolve(ext.endsWith(".json") ? ext : ext + "/tsconfig.json", { paths: [path.dirname(file)] }); } catch {}
    }
    const parent = parentFile ? readTsconfigChain(parentFile, seen) : null;
    if (parent && parent.paths) merged = parent;
  }
  if (own.paths) {
    merged = { paths: own.paths, pathsBase: path.resolve(path.dirname(file), own.baseUrl || ".") };
  } else if (own.baseUrl && merged.paths) {
    merged = { paths: merged.paths, pathsBase: path.resolve(path.dirname(file), own.baseUrl) };
  }
  return merged;
}

function loadTsconfigAliases(dir) {
  for (const name of ["tsconfig.json", "tsconfig.app.json"]) {
    const chain = readTsconfigChain(path.join(dir, name));
    if (!chain || !chain.paths) continue;
    const base = chain.pathsBase;
    const out = [];
    for (const [key, targets] of Object.entries(chain.paths)) {
      if (!Array.isArray(targets) || !targets.length) continue;
      // Every target, in order: the first that resolves wins (tsc fallback order).
      for (const t of targets) {
        if (typeof t !== "string") continue;
        if (key.endsWith("/*") && t.endsWith("/*")) {
          out.push({ prefix: key.slice(0, -1), target: path.resolve(base, t.slice(0, -1)) });
        } else if (!key.endsWith("/*") && !t.endsWith("/*")) {
          out.push({ exact: key, target: path.resolve(base, t) });
        }
      }
    }
    if (out.length) return out;
  }
  return [];
}

function aliasResolve(id) {
  for (const a of aliasEntries) {
    if (a.exact && id === a.exact) return a.target;
    if (a.prefix && id.startsWith(a.prefix)) return path.join(a.target, id.slice(a.prefix.length));
  }
  return null;
}

// Match Vite's esbuildDepPlugin: never let esbuild bundle a non-JS type into a
// dep. Styles, preprocessors, wasm, single-file-component types, and known asset
// types (fonts, images, media) are externalized — a relative one resolved to an
// absolute path so its URL is correct, a bare/absolute one kept as-is — and left
// for oj's own asset/CSS pipeline to serve. Without this a dep whose CSS pulls
// in a `.woff2` fails the entire pre-bundle ("No loader is configured").
const EXTERNAL_TYPES = [
  "css", "scss", "sass", "less", "styl", "stylus", "pcss", "postcss",
  "wasm",
  "vue", "svelte", "astro", "imba", "marko",
  "png", "jpg", "jpeg", "gif", "svg", "ico", "webp", "avif", "bmp", "cur",
  "woff", "woff2", "ttf", "otf", "eot",
  "mp4", "webm", "ogg", "mp3", "wav", "flac", "aac", "mov", "m4a",
];
const EXTERNAL_RE = new RegExp(`\\.(${EXTERNAL_TYPES.join("|")})(\\?.*)?$`, "i");
const externalizeNonJs = {
  name: "oj-externalize-non-js",
  setup(build) {
    build.onResolve({ filter: EXTERNAL_RE }, (args) => {
      if (args.path.startsWith(".")) {
        return { path: path.resolve(args.resolveDir, args.path), external: true };
      }
      return { path: args.path, external: true };
    });
  },
};

async function scan() {
  const found = new Set();
  const collector = {
    name: "oj-scan",
    setup(build) {
      build.onResolve({ filter: /.*/ }, async (args) => {
        if (args.kind === "entry-point") return null;
        const aliased = aliasResolve(args.path);
        if (aliased) {
          const r = await build.resolve(aliased, { kind: args.kind, resolveDir: root });
          if (r.errors && r.errors.length) return null;
          return { path: r.path, external: r.external };
        }
        if (isBare(args.path) && !excludeSet.has(args.path)) {
          // A specifier with a query (`x?worker`, `x?url`, `x?raw`) is a special
          // import handled by oj's worker/asset pipeline, not a plain dep — never
          // pre-bundle it (the optimized-dep URL would 404). Externalize it so it
          // is served directly. Vite excludes queried imports from the optimizer.
          if (!args.path.includes("?")) {
            found.add(args.path);
          }
          return { path: args.path, external: true };
        }
        if (!args.path.startsWith(".") && !path.isAbsolute(args.path)) return { path: args.path, external: true };
        return null;
      });
    },
  };
  try {
    await esbuild.build({
      jsx: "automatic",
      ...esbuildOptions,
      entryPoints: entryList,
      absWorkingDir: root,
      bundle: true,
      write: false,
      logLevel: "silent",
      platform: esbuildOptions.platform ?? "browser",
      loader: { ".js": "jsx", ".ts": "ts", ".tsx": "tsx", ".jsx": "jsx", ...(esbuildOptions.loader ?? {}) },
      plugins: [...(esbuildOptions.plugins ?? []), externalizeNonJs, collector],
      metafile: false,
    });
  } catch {}
  return found;
}

/// Runs the dep pre-bundle and returns `{ metadata }`. Called by oj through a
/// short-lived in-process JS engine; the config arrives as a JSON argument and
/// the metadata leaves as the return value (no argv, no stdout), so neither an
/// oversized include list nor a dep that prints on require can break the
/// channel.
export async function optimize(input) {
  ({ root, outDir, entries, include = [], exclude = [], dedupe = [], alias = [], autoDiscover = false, resolve: resolveSettings = {} } = input);
  NEEDS_INTEROP = new Set(input.needsInterop ?? []);
  resolveConditions = (resolveSettings.conditions ?? ["browser", "module", "import", "development"])
    .filter((c) => !ESBUILD_IMPLICIT_CONDITIONS.has(c));
  esbuildOptions = Object.fromEntries(
    Object.entries(input.esbuildOptions ?? {}).filter(([k]) => ESBUILD_OPTION_KEYS.has(k)),
  );
  DEDUPE = new Set([
    "react",
    "react-dom",
    "react-dom/client",
    "react/jsx-runtime",
    "react/jsx-dev-runtime",
    ...dedupe,
  ]);
  req = createRequire(path.join(root, "package.json"));
  excludeSet = new Set(exclude);
  includeIds = [];
  for (const inc of include) {
    for (const id of isDynamicPattern(inc) ? expandGlobIds(inc) : [inc]) {
      if (!includeIds.includes(id)) includeIds.push(id);
    }
  }
  // optimizeDeps.entries are glob patterns relative to root (Vite scans them with
  // tinyglobby); a literal path is used as given.
  entryList =
    entries && entries.length
      ? entries.flatMap((e) =>
          isDynamicPattern(e)
            ? globFiles(e, root)
                .filter((f) => !f.split("/").includes("node_modules"))
                .map((f) => path.join(root, f))
            : [path.isAbsolute(e) ? e : path.join(root, e)],
        )
      : detectEntries();
  DEDUPE_PKGS = new Set(dedupe.map(npmPackageName).filter(Boolean));
  aliasEntries = [
    ...loadTsconfigAliases(root),
    ...(alias || []).map(([find, replacement]) => ({ exact: find, prefix: find + "/", target: replacement })),
  ];
  esbuild = await import(pathToFileURL(resolveEsbuild()).href).then((m) => m.default ?? m);

// Only pre-bundle the explicit optimizeDeps.include list by default (a small,
// author-vetted, well-behaved set). Full-graph auto-discovery via the esbuild
// scan is opt-in: converting an entire app's CommonJS/UMD dependency tree to ESM
// with esbuild has a real interop tail (UMD `this`->void 0, cross-dep shape) that
// can break an app, so oj's robust per-module wrap_cjs serves undiscovered deps
// instead. See oj-native partial bundling for the eventual request-count fix.
const scanned = autoDiscover ? await scan() : new Set();
for (const inc of includeIds) scanned.add(inc);
const deps = [...scanned].filter((d) => !excludeSet.has(d));

const entryPoints = {};
const nameOf = {};
for (const dep of deps) {
  // Vite's nested-dependency syntax: "a > b" pre-bundles the copy of `b` nested
  // inside `a` (each segment resolved from the previous one's directory). The
  // optimized dep registers under the LAST segment, which is what the app imports;
  // esbuild gets the concrete resolved file for that nested copy. A broken nested
  // include is dropped, not allowed to fail the whole pre-bundle.
  if (dep.includes(">")) {
    const parts = dep.split(">").map((s) => s.trim()).filter(Boolean);
    let resolved;
    let fromDir;
    try {
      for (const part of parts) {
        resolved = fromDir ? req.resolve(part, { paths: [fromDir] }) : req.resolve(part);
        fromDir = path.dirname(resolved);
      }
    } catch {
      continue;
    }
    const last = parts[parts.length - 1];
    const name = last.replace(/^@/, "").replace(/[^\w.-]/g, "_");
    entryPoints[name] = resolved;
    nameOf[last] = name;
    continue;
  }
  // A package.json `#imports` subpath (e.g. `#shared/i18n/compiled/messages`)
  // resolves to a file INSIDE the project, not a node_modules dep. Vite's optimizer
  // targets node_modules; it does not pre-bundle project source, and neither should
  // oj. These are served as source and handled by the app's own plugins (the
  // i18n-dev `load` hook collapses the message barrel into grouped virtual modules),
  // so pre-bundling them would both fight that plugin and split the module instance
  // between SSR and client.
  if (dep.startsWith("#")) continue;
  let entry;
  try {
    entry = req.resolve(dep);
  } catch {
    continue;
  }
  // Skip linked / workspace packages (symlinked into node_modules): pre-bundling
  // freezes their source so edits to them stop HMR-ing. Vite excludes these too.
  // An explicit optimizeDeps.include still forces optimization.
  if (!includeIds.includes(dep)) {
    let real = entry;
    try {
      real = realpathSync(entry);
    } catch {}
    if (!real.split(path.sep).includes("node_modules")) {
      continue;
    }
  }
  // The lingui macro entrypoints are served by oj's runtime shim, never bundled:
  // pre-bundling them would drag in the whole babel macro toolchain (which
  // imports node builtins) and, worse, route the specifier to the optimized dep
  // instead of the shim. oj externalizes these at serve time too.
  if (/^@lingui\/(macro|core\/macro|react\/macro)$/.test(dep)) {
    continue;
  }
  // Only pre-bundle JavaScript deps. A CSS-only dep (e.g. `@fontsource/*`) is
  // not a JS module: esbuild would emit a stray .css and choke on the fonts its
  // @font-face rules pull in, failing the *whole* build. Vite excludes these
  // from the dep optimizer too; oj serves them directly through its CSS pipeline.
  // (`json` too: Vite's OPTIMIZABLE_ENTRY_RE admits only script entries, so an
  // expanded `pkg/*` include never pre-bundles `pkg/package.json`.)
  if (/\.(css|scss|sass|less|styl|woff2?|ttf|otf|eot|svg|png|jpe?g|gif|webp|avif|mp4|webm|wasm|json|html|md)$/i.test(entry)) {
    continue;
  }
  const name = dep.replace(/^@/, "").replace(/[^\w.-]/g, "_");
  // Hand esbuild the BARE specifier, not the Node-resolved path: `req.resolve`
  // picks the `require`/`node` condition (uuid's ./dist/cjs, which `require`s
  // node crypto), and bundling that fixed file skips browser resolution. A bare
  // entry lets esbuild's platform:"browser" pick the browser build. Vite does
  // the same via its browser-aware resolver in esbuildDepPlugin.
  entryPoints[name] = dep;
  nameOf[dep] = name;
}

rmSync(outDir, { recursive: true, force: true });
mkdirSync(outDir, { recursive: true });

const metadata = {};
if (Object.keys(entryPoints).length) {
  const result = await esbuild.build({
    // The Rust resolver's settings, so a dual-build dep pre-bundles the same file
    // the dev server would serve for a source import of it.
    mainFields: resolveSettings.mainFields ?? ["browser", "module", "main"],
    conditions: resolveConditions,
    ...(resolveSettings.extensions ? { resolveExtensions: resolveSettings.extensions } : {}),
    ...(resolveSettings.preserveSymlinks ? { preserveSymlinks: true } : {}),
    target: "esnext",
    ...esbuildOptions,
    entryPoints,
    absWorkingDir: root,
    bundle: true,
    splitting: true,
    format: "esm",
    outdir: outDir,
    outExtension: { ".js": ".mjs" },
    platform: esbuildOptions.platform ?? "browser",
    define: { "process.env.NODE_ENV": JSON.stringify("development"), ...(esbuildOptions.define ?? {}) },
    // A node-oriented dep (e.g. @react-pdf/renderer, cosmiconfig) may import a
    // node builtin; externalize them so one such dep can't fail the whole
    // pre-bundle. The import stays in the output and oj serves a browser stub,
    // matching Vite's esbuildDepPlugin, which also externalizes builtins.
    external: [...NODE_BUILTINS, ...(esbuildOptions.external ?? [])],
    plugins: [...(esbuildOptions.plugins ?? []), dedupeFromRoot, externalizeNonJs],
    logLevel: "silent",
    metafile: true,
    write: true,
  });
  const exportsOf = {};
  for (const [out, meta] of Object.entries(result.metafile.outputs)) {
    if (meta.entryPoint) exportsOf[path.basename(out)] = meta.exports || [];
  }
  const IDENT = /^[A-Za-z_$][A-Za-z0-9_$]*$/;
  function namedExportsOf(dep) {
    try {
      const m = req(dep);
      const mod = m && m.__esModule && m.default && typeof m.default === "object" ? m.default : m;
      if (!mod || (typeof mod !== "object" && typeof mod !== "function")) return [];
      return [...new Set(Object.keys(mod))].filter((k) => k !== "default" && k !== "__esModule" && IDENT.test(k));
    } catch {
      return [];
    }
  }
  for (const [dep, name] of Object.entries(nameOf)) {
    const file = `${name}.mjs`;
    const exports = exportsOf[file] || [];
    const bundledInterop = exports.length === 0 || (exports.length === 1 && exports[0] === "default");
    if (bundledInterop && DEDUPE.has(dep)) {
      const names = namedExportsOf(dep);
      if (names.length) {
        const cjsFile = `${name}-cjs.mjs`;
        renameSync(path.join(outDir, file), path.join(outDir, cjsFile));
        const proxy =
          `import __m from "./${cjsFile}";\n` +
          `export default __m;\n` +
          `export const __cjs_exports = __m;\n` +
          `export const { ${names.join(", ")} } = __m;\n`;
        writeFileSync(path.join(outDir, file), proxy);
        metadata[dep] = { file, needsInterop: NEEDS_INTEROP.has(dep), exports: ["default", ...names] };
        continue;
      }
    }
    metadata[dep] = { file, needsInterop: bundledInterop || NEEDS_INTEROP.has(dep), exports };
  }
}

return { metadata };
}
