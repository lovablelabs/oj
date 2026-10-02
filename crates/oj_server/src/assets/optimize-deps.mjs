// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { mkdirSync, rmSync, readFileSync, writeFileSync, existsSync, realpathSync, globSync } from "node:fs";
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
  "define",
  "target",
  "supported",
  "loader",
  "jsx",
  "jsxDev",
  "jsxSideEffects",
  "jsxFactory",
  "jsxFragment",
  "jsxImportSource",
  "mainFields",
  "conditions",
  "resolveExtensions",
  "preserveSymlinks",
  "keepNames",
  "minify",
  "minifyWhitespace",
  "minifyIdentifiers",
  "minifySyntax",
  "treeShaking",
  "platform",
  "external",
  "banner",
  "footer",
  "inject",
  "alias",
  "drop",
  "pure",
  "charset",
  "legalComments",
  "tsconfig",
  "tsconfigRaw",
  "ignoreAnnotations",
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
    return globSync(pattern, { cwd, exclude: (name) => name === "node_modules" }).map((p) =>
      p.split(path.sep).join("/"),
    );
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
        const valueRe = new RegExp(
          value
            .split("*")
            .map((s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"))
            .join("(.*)"),
        );
        for (let file of globFiles(value.replace(/\*/g, "**/*"), pkgDir)) {
          if (value.startsWith("./")) file = "./" + file;
          const m = valueRe.exec(file);
          if (!m) continue;
          let same = true;
          for (let i = 2; i < m.length; i++)
            if (m[i] !== m[i - 1]) {
              same = false;
              break;
            }
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
const NODE_BUILTINS = new Set([
  ...builtinModules.builtinModules,
  ...builtinModules.builtinModules.map((m) => "node:" + m),
]);
const isBare = (id) =>
  id && !id.startsWith(".") && !id.startsWith("/") && !id.startsWith("\0") && !NODE_BUILTINS.has(id);

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
      try {
        parentFile = req.resolve(ext.endsWith(".json") ? ext : ext + "/tsconfig.json", { paths: [path.dirname(file)] });
      } catch {}
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
  "css",
  "scss",
  "sass",
  "less",
  "styl",
  "stylus",
  "pcss",
  "postcss",
  "wasm",
  "vue",
  "svelte",
  "astro",
  "imba",
  "marko",
  "png",
  "jpg",
  "jpeg",
  "gif",
  "svg",
  "ico",
  "webp",
  "avif",
  "bmp",
  "cur",
  "woff",
  "woff2",
  "ttf",
  "otf",
  "eot",
  "mp4",
  "webm",
  "ogg",
  "mp3",
  "wav",
  "flac",
  "aac",
  "mov",
  "m4a",
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

async function esbuildScan() {
  const found = new Map();
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
            found.set(args.path, null);
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

const tryResolve = (r, spec) => {
  try {
    return r.resolve(spec);
  } catch {
    return null;
  }
};

/// The bundler Vite itself would pre-bundle this app with: Vite 8's optimizer
/// runs on rolldown and Vite <= 7's on esbuild, so the app's own `vite`
/// decides and its copy of that bundler builds. An app without Vite gets the
/// rolldown vendored next to oj, then whichever bundler it installs itself.
export function pickBundler(appRoot, vendoredRolldown) {
  const appReq = createRequire(path.join(appRoot, "package.json"));
  const sources = [];
  const viteJson = tryResolve(appReq, "vite/package.json");
  if (viteJson) {
    const viteReq = createRequire(viteJson);
    sources.push(["rolldown", viteReq, "vite"], ["esbuild", viteReq, "vite"]);
  }
  if (vendoredRolldown) sources.push(["rolldown", createRequire(path.join(vendoredRolldown, "package.json")), "oj"]);
  sources.push(["rolldown", appReq, "app"], ["esbuild", appReq, "app"]);
  for (const [kind, from, via] of sources) {
    const entry = tryResolve(from, kind);
    if (entry) return { kind, entry, require: from, via };
  }
  throw new Error(
    "no dependency bundler found (neither rolldown nor esbuild, directly or via vite); dep pre-bundling skipped",
  );
}

const importFile = async (file) => import(pathToFileURL(file).href);

async function loadRolldown(bundler) {
  const main = await importFile(bundler.entry);
  const experimental = await importFile(bundler.require.resolve("rolldown/experimental"));
  const utilsPath = tryResolve(bundler.require, "rolldown/utils");
  const utils = utilsPath ? await importFile(utilsPath).catch(() => null) : null;
  return {
    rolldown: main.rolldown,
    scan: experimental.scan,
    parseSync: utils?.parseSync ?? experimental.parseSync,
  };
}

// Vite's constants: what the scanner follows, what the optimizer bundles,
// which queries are left to the dev server, and its default build target.
const JS_TYPES_RE = /\.(?:j|t)sx?$|\.mjs$/;
const OPTIMIZABLE_ENTRY_RE = /\.[cm]?[jt]s$/;
const SPECIAL_QUERY_RE = /[?&](?:worker|sharedworker|raw|url)\b/;
const SCAN_EXTERNAL_RE = /\.(?:json|json5|wasm)$/;
const BASELINE_TARGET = ["chrome111", "edge111", "firefox114", "safari16.4", "ios16.4"];
const BARE_RE = /^[\w@][^:]/;
const EXTERNAL_URL_RE = /^(?:[a-z]+:)?\/\//i;
const cleanUrl = (id) => id.replace(/[?#].*$/s, "");
const isInNodeModules = (id) => id.split(/[\\/]/).includes("node_modules");
const moduleListContains = (list, id) => list.some((m) => m === id || id.startsWith(m + "/"));

let realRoot;
/// The URL oj's dev server serves a file at (rewrite.rs `url_of`): root-relative
/// inside the root, `/@fs` outside it. Bundles are served verbatim, so every
/// import they keep must already be a URL the browser can fetch.
export function urlOf(file) {
  for (const base of [root, realRoot]) {
    const rel = path.relative(base, file);
    if (rel && !rel.startsWith("..") && !path.isAbsolute(rel)) return "/" + rel.split(path.sep).join("/");
  }
  return "/@fs" + file.split(path.sep).join("/");
}

// Vite's optionalPeerDepId: a bare import a dep's package.json lists as an
// optional peer, and which is not installed, becomes a module that throws
// when evaluated instead of failing the whole pre-bundle.
function optionalPeerOf(id, importer) {
  const pkgName = npmPackageName(id);
  if (!pkgName || !importer || !isInNodeModules(importer)) return null;
  let dir = path.dirname(importer);
  for (;;) {
    const pj = path.join(dir, "package.json");
    if (existsSync(pj)) {
      try {
        const meta = JSON.parse(readFileSync(pj, "utf8"));
        if (!meta.name) throw 0;
        return meta.peerDependenciesMeta?.[pkgName]?.optional ? `${pkgName}:${meta.name}` : null;
      } catch (e) {
        if (e !== 0) return null;
      }
    }
    const parent = path.dirname(dir);
    if (parent === dir) return null;
    dir = parent;
  }
}

const SCAN_INCLUDE_ID = "\0oj-scan-include";

/// Vite 8's scanImports (optimizer/scan.ts) on rolldown's `scan`: crawl the
/// app from its entries, record every bare import that resolves into
/// node_modules (or is listed in `include`) and stop there, keep crawling into
/// linked packages, externalize everything that is not JS. The include list
/// rides along as one virtual entry, so every pre-bundled dep is resolved by
/// the same resolver the bundle uses (Vite's addManuallyIncludedOptimizeDeps).
async function rolldownScan(rd, discover) {
  const found = new Map();
  const input = [...(discover ? entryList : [])];
  const plainIncludes = includeIds.filter((i) => !i.includes(">"));
  if (plainIncludes.length) input.push(SCAN_INCLUDE_ID);
  if (!input.length) return found;
  const includeImporter = path.join(root, "__oj_include__.js");
  const seen = new Map();
  const resolve = async (ctx, id, importer) => {
    const from = importer === SCAN_INCLUDE_ID ? includeImporter : importer;
    const key = `${id}\0${from ? path.dirname(from) : ""}`;
    if (seen.has(key)) return seen.get(key);
    let out = null;
    try {
      const r = await ctx.resolve(aliasResolve(id) ?? id, from, { skipSelf: true });
      if (r && !r.external) out = r.id;
    } catch {}
    seen.set(key, out);
    return out;
  };
  const externalize = (id) => ({ id, external: true });
  const plugin = {
    name: "oj:dep-scan",
    resolveId: {
      async handler(id, importer) {
        if (id === SCAN_INCLUDE_ID) return id;
        if (!importer) return null;
        if (EXTERNAL_URL_RE.test(id) || id.startsWith("data:")) return externalize(id);
        if (SPECIAL_QUERY_RE.test(id)) return externalize(id);
        if (BARE_RE.test(id) && !aliasResolve(id)) {
          if (id.includes("?") || moduleListContains(exclude, id) || found.has(id)) return externalize(id);
          const resolved = await resolve(this, id, importer);
          if (!resolved || !path.isAbsolute(resolved) || resolved.includes("\0")) return externalize(id);
          if (isInNodeModules(resolved) || includeIds.includes(id)) {
            if (OPTIMIZABLE_ENTRY_RE.test(cleanUrl(resolved))) found.set(id, resolved);
            return externalize(id);
          }
          return JS_TYPES_RE.test(cleanUrl(resolved)) ? resolved : externalize(id);
        }
        if (EXTERNAL_RE.test(id) || SCAN_EXTERNAL_RE.test(cleanUrl(id))) return externalize(id);
        const resolved = await resolve(this, id, importer);
        if (resolved && path.isAbsolute(resolved) && JS_TYPES_RE.test(cleanUrl(resolved))) return cleanUrl(resolved);
        return externalize(id);
      },
    },
    load: {
      handler(id) {
        if (id !== SCAN_INCLUDE_ID) return null;
        return { code: plainIncludes.map((i) => `import ${JSON.stringify(i)};`).join("\n"), moduleType: "js" };
      },
    },
  };
  try {
    await rd.scan({
      input,
      cwd: root,
      logLevel: "silent",
      platform: esbuildOptions.platform ?? "browser",
      plugins: [plugin],
      resolve: rolldownResolveOptions(),
      transform: { jsx: { runtime: "automatic", development: true } },
      // oj compiles JSX in plain .js app files; the scan must parse them too.
      moduleTypes: { ".js": "jsx" },
    });
  } catch (e) {
    console.warn(`oj: dependency scan stopped early (${String(e?.message ?? e).split("\n")[0]})`);
  }
  return found;
}

function rolldownResolveOptions() {
  const extensions = esbuildOptions.resolveExtensions ?? resolveSettings.extensions;
  return {
    mainFields: esbuildOptions.mainFields ?? resolveSettings.mainFields ?? ["browser", "module", "main"],
    conditionNames: esbuildOptions.conditions ?? resolveConditions,
    ...(extensions ? { extensions } : {}),
    ...(resolveSettings.preserveSymlinks ? { symlinks: false } : {}),
    ...(esbuildOptions.alias ? { alias: esbuildOptions.alias } : {}),
  };
}

const BROWSER_EXTERNAL = "\0oj-dep:browser-external:";
const OPTIONAL_PEER = "\0oj-dep:optional-peer:";
const CONVERT_EXTERNAL = "\0oj-dep:external-conversion:";
const CONVERTED_PREFIX = "oj-dep-external:";
const ASSET_IMPORT_META_URL_RE = /\bnew\s+URL\s*\(\s*('[^']+'|"[^"]+"|`[^`]+`)\s*,\s*import\.meta\.url\s*(?:,\s*)?\)/g;

/// Vite's rolldownDepPlugin, with one difference: oj serves the bundle
/// verbatim (Vite runs importAnalysis over it), so whatever stays external
/// is written as the URL oj serves it at instead of a path or bare name.
function rolldownDepPlugins() {
  const rootImporter = path.join(root, "package.json");
  const rootDirs = new Set([root, realRoot]);
  return [
    {
      name: "oj:dep-pre-bundle-assets",
      resolveId: {
        filter: { id: EXTERNAL_RE },
        async handler(id, importer, opts) {
          if (id.startsWith(CONVERTED_PREFIX)) return { id: id.slice(CONVERTED_PREFIX.length), external: "absolute" };
          if (!importer) return null;
          const r = await this.resolve(id, importer, { skipSelf: true });
          let file = r && !r.external ? r.id : null;
          if (file && JS_TYPES_RE.test(cleanUrl(file))) return r;
          if (!file && id.startsWith(".")) file = path.resolve(path.dirname(importer), id);
          const url = file ? urlOf(file) : id;
          // require() of an external stays a runtime require in rolldown's
          // output; Vite converts it to an import through a facade module.
          if (opts?.kind === "require-call") return { id: CONVERT_EXTERNAL + url };
          return { id: url, external: "absolute" };
        },
      },
      load: {
        filter: { id: new RegExp(`^${CONVERT_EXTERNAL}`) },
        handler(id) {
          const url = id.slice(CONVERT_EXTERNAL.length);
          const spec = JSON.stringify(CONVERTED_PREFIX + url);
          return /\.(css|scss|sass|less|styl|stylus|pcss|postcss)(\?|$)/.test(url) && !/\.module\./.test(url)
            ? `import ${spec};`
            : `export { default } from ${spec};\nexport * from ${spec};`;
        },
      },
    },
    {
      name: "oj:dep-pre-bundle",
      resolveId: {
        filter: { id: BARE_RE },
        async handler(id, importer) {
          if (!importer) return null;
          if (moduleListContains(exclude, id)) {
            const r = await this.resolve(id, importer, { skipSelf: true });
            return { id: r && !r.external ? urlOf(r.id) : id, external: "absolute" };
          }
          const aliased = aliasResolve(id);
          if (aliased) {
            const r = await this.resolve(aliased, importer, { skipSelf: true });
            if (r) return r;
          }
          // resolve.dedupe: resolve from the project root wherever the
          // importer sits (Vite resolve.ts: dedupe -> basedir = root).
          const fromRoot = DEDUPE_PKGS.has(npmPackageName(id)) && !rootDirs.has(path.dirname(importer));
          const r = await this.resolve(id, fromRoot ? rootImporter : importer, { skipSelf: true });
          if (r) return r;
          if (NODE_BUILTINS.has(id)) return { id: BROWSER_EXTERNAL + id };
          const peer = optionalPeerOf(id, importer);
          if (peer) return { id: OPTIONAL_PEER + peer };
          return null;
        },
      },
      load: {
        filter: { id: /^\0oj-dep:(browser-external|optional-peer):/ },
        handler(id) {
          if (id.startsWith(BROWSER_EXTERNAL)) {
            const name = id.slice(BROWSER_EXTERNAL.length);
            return (
              `module.exports = Object.create(new Proxy({}, {\n` +
              `  get(_, key) {\n` +
              `    if (key !== "__esModule" && key !== "__proto__" && key !== "constructor" && key !== "splice") {\n` +
              `      console.warn(${JSON.stringify(`Module "${name}" has been externalized for browser compatibility. Cannot access "${name}.`)} + String(key) + '" in client code. See https://vite.dev/guide/troubleshooting.html#module-externalized-for-browser-compatibility for more details.');\n` +
              `    }\n` +
              `  }\n` +
              `}));\n`
            );
          }
          const [peerDep, parentDep] = id.slice(OPTIONAL_PEER.length).split(":");
          return (
            "module.exports = {};" +
            `throw new Error(${JSON.stringify(`Could not resolve "${peerDep}" imported by "${parentDep}". Is it installed?`)});`
          );
        },
      },
      // `new URL("./x.wasm", import.meta.url)` in a dep is relative to the
      // dep's own file; the bundle lives elsewhere, so point it at the URL oj
      // serves that file at (Vite rewrites it relative to its deps dir).
      transform: {
        filter: { code: /import\.meta\.url/ },
        handler(code, id) {
          let changed = false;
          const out = code.replace(ASSET_IMPORT_META_URL_RE, (m, raw) => {
            if (raw[0] === "`" && raw.includes("${")) return m;
            const url = raw.slice(1, -1);
            if (url.startsWith("data:") || url.startsWith("/") || EXTERNAL_URL_RE.test(url)) return m;
            changed = true;
            return `new URL('' + ${JSON.stringify(urlOf(path.resolve(path.dirname(id), url)))}, import.meta.url)`;
          });
          return changed ? { code: out } : null;
        },
      },
    },
  ];
}

const isSingleDefaultExport = (exports) => exports.length === 1 && exports[0] === "default";

/// Vite's needsInterop (optimizer/index.ts) minus the forced list: the entry
/// has no ESM syntax (CJS/UMD), or the bundle collapsed an ESM entry into a
/// lone default export (a peer require()d it).
function entryNeedsInterop(parseSync, facade, generated) {
  let parsed = null;
  if (parseSync && facade && path.isAbsolute(facade)) {
    try {
      parsed = parseSync(facade, readFileSync(facade, "utf8")).module;
    } catch {}
  }
  if (!parsed) return generated.length === 0 || isSingleDefaultExport(generated);
  if (!parsed.hasModuleSyntax) return true;
  const entryExports = [];
  for (const exp of parsed.staticExports ?? []) {
    for (const e of exp.entries ?? []) {
      if (e.exportName?.kind === "Default") entryExports.push("default");
      else if (e.exportName?.name) entryExports.push(e.exportName.name);
    }
  }
  return isSingleDefaultExport(generated) && !isSingleDefaultExport(entryExports);
}

/// Vite's prepareRolldownOptimizerRun: one rolldown build, one entry per dep
/// plus shared chunks, ESM. Returns `{ [name]: { file, exports, cjs } }`.
async function rolldownBundle(rd, entryPoints) {
  const bundle = await rd.rolldown({
    input: entryPoints,
    cwd: root,
    logLevel: "silent",
    platform: esbuildOptions.platform ?? "browser",
    plugins: rolldownDepPlugins(),
    ...(esbuildOptions.external ? { external: esbuildOptions.external } : {}),
    transform: {
      target: esbuildOptions.target ?? BASELINE_TARGET,
      define: { "process.env.NODE_ENV": JSON.stringify("development"), ...(esbuildOptions.define ?? {}) },
    },
    resolve: rolldownResolveOptions(),
  });
  let output;
  try {
    ({ output } = await bundle.write({
      format: "esm",
      dir: outDir,
      entryFileNames: "[name].mjs",
      chunkFileNames: "[name]-[hash].mjs",
      sourcemap: false,
    }));
  } finally {
    await bundle.close();
  }
  const built = {};
  for (const chunk of output) {
    if (chunk.type !== "chunk" || !chunk.isEntry) continue;
    const exports = chunk.exports ?? [];
    built[chunk.name] = {
      file: chunk.fileName,
      exports,
      cjs: entryNeedsInterop(rd.parseSync, chunk.facadeModuleId, exports),
    };
  }
  return built;
}

async function esbuildBundle(entryPoints) {
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
    // pre-bundle, matching Vite's esbuildDepPlugin.
    external: [...NODE_BUILTINS, ...(esbuildOptions.external ?? [])],
    plugins: [...(esbuildOptions.plugins ?? []), dedupeFromRoot, externalizeNonJs],
    logLevel: "silent",
    metafile: true,
    write: true,
  });
  const built = {};
  for (const [out, meta] of Object.entries(result.metafile.outputs)) {
    if (!meta.entryPoint) continue;
    const file = path.basename(out);
    const exports = meta.exports || [];
    built[file.replace(/\.mjs$/, "")] = {
      file,
      exports,
      cjs: exports.length === 0 || isSingleDefaultExport(exports),
    };
  }
  return built;
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

/// Runs the dep pre-bundle and returns `{ metadata, bundler }`. Called by oj
/// through a short-lived JS engine; the config arrives as a JSON argument and
/// the metadata leaves as the return value (no argv, no stdout), so neither an
/// oversized include list nor a dep that prints on require can break the
/// channel.
export async function optimize(input) {
  ({
    root,
    outDir,
    entries,
    include = [],
    exclude = [],
    dedupe = [],
    alias = [],
    autoDiscover = true,
    resolve: resolveSettings = {},
  } = input);
  try {
    realRoot = realpathSync(root);
  } catch {
    realRoot = root;
  }
  NEEDS_INTEROP = new Set(input.needsInterop ?? []);
  resolveConditions = (resolveSettings.conditions ?? ["browser", "module", "import", "development"]).filter(
    (c) => !ESBUILD_IMPLICIT_CONDITIONS.has(c),
  );
  esbuildOptions = Object.fromEntries(
    Object.entries(input.esbuildOptions ?? {}).filter(([k]) => ESBUILD_OPTION_KEYS.has(k)),
  );
  DEDUPE = new Set(["react", "react-dom", "react-dom/client", "react/jsx-runtime", "react/jsx-dev-runtime", ...dedupe]);
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

  const bundler = pickBundler(root, input.vendoredRolldown);
  let rd = null;
  let candidates;
  if (bundler.kind === "rolldown") {
    rd = await loadRolldown(bundler);
    candidates = await rolldownScan(rd, autoDiscover);
    // "a > b" names a nested copy the scan cannot resolve; it resolves below.
    for (const inc of includeIds) if (inc.includes(">") && !candidates.has(inc)) candidates.set(inc, null);
  } else {
    esbuild = await importFile(bundler.entry).then((m) => m.default ?? m);
    // optimizeDeps.noDiscovery: only the include list (Vite parity).
    candidates = autoDiscover ? await esbuildScan() : new Map();
    for (const inc of includeIds) if (!candidates.has(inc)) candidates.set(inc, null);
  }
  const deps = [...candidates.keys()].filter((d) => !excludeSet.has(d));

  const entryPoints = {};
  const nameOf = {};
  for (const dep of deps) {
    // Vite's nested-dependency syntax: "a > b" pre-bundles the copy of `b` nested
    // inside `a` (each segment resolved from the previous one's directory). The
    // optimized dep registers under the LAST segment, which is what the app imports;
    // the bundler gets the concrete resolved file for that nested copy. A broken
    // nested include is dropped, not allowed to fail the whole pre-bundle.
    if (dep.includes(">")) {
      const parts = dep
        .split(">")
        .map((s) => s.trim())
        .filter(Boolean);
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
    // A package.json `#imports` subpath resolves to a file INSIDE the project, not
    // a node_modules dep; Vite does not pre-bundle project source and neither does
    // oj (it would also split the module instance between SSR and client).
    if (dep.startsWith("#")) continue;
    // rolldown entries are resolved by the scan with the bundle's own resolver;
    // one that did not resolve there would fail the whole build.
    const scanned = candidates.get(dep);
    if (bundler.kind === "rolldown" && !scanned) continue;
    const entry = scanned ?? tryResolve(req, dep);
    if (!entry) continue;
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
    // pre-bundling them would drag in the whole babel macro toolchain and route
    // the specifier to the optimized dep instead of the shim.
    if (/^@lingui\/(macro|core\/macro|react\/macro)$/.test(dep)) {
      continue;
    }
    // Only pre-bundle JavaScript deps (Vite's OPTIMIZABLE_ENTRY_RE): a CSS-only dep
    // (e.g. `@fontsource/*`) or an expanded `pkg/*` include's package.json is
    // served directly through oj's own pipelines.
    if (
      /\.(css|scss|sass|less|styl|woff2?|ttf|otf|eot|svg|png|jpe?g|gif|webp|avif|mp4|webm|wasm|json|html|md)$/i.test(
        entry,
      )
    ) {
      continue;
    }
    const name = dep.replace(/^@/, "").replace(/[^\w.-]/g, "_");
    // esbuild gets the BARE specifier, not the Node-resolved path: `req.resolve`
    // picks the `require`/`node` condition (uuid's ./dist/cjs), while a bare entry
    // lets esbuild's platform:"browser" pick the browser build. rolldown gets the
    // file its own browser resolver picked during the scan (Vite's flatIdDeps).
    entryPoints[name] = bundler.kind === "rolldown" ? scanned : dep;
    nameOf[dep] = name;
  }

  rmSync(outDir, { recursive: true, force: true });
  mkdirSync(outDir, { recursive: true });

  const metadata = {};
  if (Object.keys(entryPoints).length) {
    const built =
      bundler.kind === "rolldown" ? await rolldownBundle(rd, entryPoints) : await esbuildBundle(entryPoints);
    for (const [dep, name] of Object.entries(nameOf)) {
      const out = built[name];
      if (!out) continue;
      // A CJS react-family dep gets a named-export facade next to its bundle,
      // so modules that link it as ESM (no interop rewrite) still find names.
      if (out.cjs && DEDUPE.has(dep)) {
        const names = namedExportsOf(dep);
        if (names.length) {
          const facade = `${name}__oj_named.mjs`;
          writeFileSync(
            path.join(outDir, facade),
            `import __m from "./${out.file}";\n` +
              `export default __m;\n` +
              `export const __cjs_exports = __m;\n` +
              `export const { ${names.join(", ")} } = __m;\n`,
          );
          metadata[dep] = { file: facade, needsInterop: NEEDS_INTEROP.has(dep), exports: ["default", ...names] };
          continue;
        }
      }
      metadata[dep] = { file: out.file, needsInterop: out.cjs || NEEDS_INTEROP.has(dep), exports: out.exports };
    }
  }

  return { metadata, bundler: bundler.kind };
}
