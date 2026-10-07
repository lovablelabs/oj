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
// Vite's `process.env.NODE_ENV || mode`, defined into every dep bundle.
let nodeEnv;
let resolveSettings;
// optimizeDeps.needsInterop: Vite's needsInterop() returns true for these before
// looking at the bundle's export shape, so the metadata must say so too.
let NEEDS_INTEROP;
let resolveConditions;
let esbuildOptions;
// optimizeDeps.rolldownOptions minus plugins and output (Vite spreads the rest
// into both the scan and the bundle); output options go to bundle.write.
let rolldownInput;
let rolldownOutput;
let DEDUPE;
let req;
let excludeSet;
let DEDUPE_PKGS;
let aliasEntries;

// esbuild activated import/require/default itself and rejected them in
// `conditions`; the filtered list stays the resolver contract.
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

// Vite's computeEntries fallbacks, in its order: the config's build inputs,
// else every html file under the root (node_modules, the build outDir, tests
// and coverage ignored, as globEntries does). The scan itself extracts the
// html files' module scripts.
function detectEntries() {
  if (buildInputs.length) {
    return buildInputs.map((e) => (path.isAbsolute(e) ? e : path.join(root, e)));
  }
  const ignored = new Set(["node_modules", buildOutDir, "__tests__", "coverage"]);
  return globFiles("**/*.html", root)
    .filter((f) => !f.split("/").some((seg) => ignored.has(seg)))
    .map((f) => path.join(root, f));
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
let warnIncludes;
let entryList;
let buildInputs;
let buildOutDir;

const NODE_BUILTINS = new Set([
  ...builtinModules.builtinModules,
  ...builtinModules.builtinModules.map((m) => "node:" + m),
]);

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
  "apng",
  "png",
  "jpg",
  "jpeg",
  "jfif",
  "pjpeg",
  "pjp",
  "gif",
  "svg",
  "ico",
  "webp",
  "avif",
  "bmp",
  "cur",
  "jxl",
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
  "opus",
  "mov",
  "m4a",
  "vtt",
  "webmanifest",
  "pdf",
  "txt",
];
const EXTERNAL_RE = new RegExp(`\\.(${EXTERNAL_TYPES.join("|")})(\\?.*)?$`, "i");

const tryResolve = (r, spec) => {
  try {
    return r.resolve(spec);
  } catch {
    return null;
  }
};

/// The rolldown this app pre-bundles with, the only engine (Vite 8 dropped
/// its esbuild optimizer; so does oj): a Vite 8 app's own vite brings one
/// (only when vite DECLARES it — resolving from vite's directory also walks
/// up into the app's node_modules, and a hoisted standalone rolldown must
/// not win), any other app gets the rolldown vendored next to oj, then one
/// the app installs itself.
export function pickRolldown(appRoot, vendoredRolldown) {
  const appReq = createRequire(path.join(appRoot, "package.json"));
  const sources = [];
  const viteJson = tryResolve(appReq, "vite/package.json");
  if (viteJson) {
    let viteDeps = {};
    try {
      viteDeps = JSON.parse(readFileSync(viteJson, "utf8")).dependencies ?? {};
    } catch {}
    if (viteDeps.rolldown) sources.push(createRequire(viteJson));
  }
  if (vendoredRolldown) sources.push(createRequire(path.join(vendoredRolldown, "package.json")));
  sources.push(appReq);
  for (const from of sources) {
    const entry = tryResolve(from, "rolldown");
    if (entry) return { entry, require: from };
  }
  throw new Error("no rolldown found (via vite, vendored, or installed); dep pre-bundling skipped");
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
const HTML_SCRIPT_ID = "\0oj-html-script:";
// Vite's scan.ts html extraction (html only: oj's other pipelines own the
// vue/svelte html-likes), run over comment-blanked markup.
const SCRIPT_RE = /(<script(?:\s+[a-z_:][-\w:]*(?:\s*=\s*(?:"[^"]*"|'[^']*'|[^"'<>=\s]+))?)*\s*>)(.*?)<\/script>/gis;
const HTML_COMMENT_RE = /<!--.*?-->/gs;
const SRC_RE = /\bsrc\s*=\s*(?:"([^"]+)"|'([^']+)'|([^\s'">]+))/i;
const TYPE_RE = /\btype\s*=\s*(?:"([^"]+)"|'([^']+)'|([^\s'">]+))/i;

// Vite's scan expands `import.meta.glob` (its js-glob transform and the html
// script path both run transformGlobImport): the matched files must be
// crawled, or a dep only a glob-routed page imports is never discovered.
// Call sites are found on literal-masked code and the patterns read from the
// raw source. Pattern shapes follow Vite's toAbsoluteGlob: `/x` from the
// root, `./x` and `../x` from the importer, `**` as-is, `!` negates; alias
// and `#subpath` patterns need the full resolver and are skipped here.
const GLOB_CALL_RE = /\bimport\.meta\.glob(?:<[^(]*>)?\s*\(/dg;

/// The string literals of the call's first argument, from `from` (just past
/// the opening paren): one literal, or an array of them. Anything dynamic
/// (an expression, a template with `${`) yields none.
function parseGlobPatterns(code, from) {
  let i = from;
  const ws = () => {
    while (i < code.length && /\s/.test(code[i])) i++;
  };
  const literal = () => {
    const q = code[i];
    if (q !== "'" && q !== '"' && q !== "`") return null;
    let out = "";
    for (i++; i < code.length && code[i] !== q; i++) {
      if (code[i] === "\\") out += code[++i] ?? "";
      else if (q === "`" && code[i] === "$" && code[i + 1] === "{") return null;
      else out += code[i];
    }
    i++;
    return out;
  };
  ws();
  if (code[i] !== "[") {
    const one = literal();
    return one == null ? [] : [one];
  }
  i++;
  const out = [];
  for (;;) {
    ws();
    if (code[i] === "]" || i >= code.length) return out;
    const l = literal();
    if (l == null) return [];
    out.push(l);
    ws();
    if (code[i] === ",") i++;
    else if (code[i] !== "]") return [];
  }
}

/// The files every `import.meta.glob` in `code` matches, absolute and sorted,
/// the importer itself excluded (Vite filters it too).
function globImportTargets(code, importer) {
  const toPosix = (p) => p.split(path.sep).join("/");
  const masked = maskLiterals(code);
  const dir = toPosix(path.dirname(importer));
  const positive = [];
  const negative = [];
  GLOB_CALL_RE.lastIndex = 0;
  for (let m; (m = GLOB_CALL_RE.exec(masked));) {
    for (const raw of parseGlobPatterns(code, m.indices[0][1])) {
      const neg = raw[0] === "!";
      const p = neg ? raw.slice(1) : raw;
      let abs;
      if (p.startsWith("/")) abs = path.posix.join(toPosix(root), p.slice(1));
      else if (p.startsWith("./") || p.startsWith("../")) abs = path.posix.join(dir, p);
      else if (p.startsWith("**")) abs = path.posix.join(toPosix(root), p);
      else continue;
      (neg ? negative : positive).push(abs);
    }
  }
  if (!positive.length) return [];
  const files = new Set();
  for (const p of positive) for (const f of globFiles(p, root)) files.add(f);
  const self = toPosix(importer);
  return [...files].filter((f) => f !== self && !negative.some((n) => path.posix.matchesGlob(f, n))).sort();
}

/// Vite 8's scanImports (optimizer/scan.ts) on rolldown's `scan`: crawl the
/// app from its entries, record every bare import that resolves into
/// node_modules (or is listed in `include`) and stop there, keep crawling into
/// linked packages, externalize everything that is not JS. The include list
/// rides along as one virtual entry, so every pre-bundled dep is resolved by
/// the same resolver the bundle uses (Vite's addManuallyIncludedOptimizeDeps).
async function rolldownScan(rd, discover, host) {
  const found = new Map();
  const input = [...(discover ? entryList : [])];
  const plainIncludes = includeIds.filter((i) => !i.includes(">"));
  if (plainIncludes.length) input.push(SCAN_INCLUDE_ID);
  if (!input.length) return found;
  const includeImporter = path.join(root, "__oj_include__.js");
  const seen = new Map();
  const resolve = async (ctx, id, importer) => {
    const from = importer === SCAN_INCLUDE_ID ? includeImporter : importer;
    // Keyed on the importer file, not its directory: a plugin may resolve the
    // same id differently per importer.
    const key = `${id}\0${from ?? ""}`;
    if (seen.has(key)) return seen.get(key);
    let out = null;
    // The app's plugins first, as Vite's scan resolves through its plugin
    // container; a plugin's virtual or external answer is not crawled.
    if (host) {
      try {
        const r = await host.resolveId(id, from);
        if (r) {
          out = !r.external && path.isAbsolute(cleanUrl(r.id)) ? r.id : null;
          seen.set(key, out);
          return out;
        }
      } catch {}
    }
    try {
      const r = await ctx.resolve(aliasResolve(id) ?? id, from, { skipSelf: true });
      if (r && !r.external) out = r.id;
    } catch {}
    seen.set(key, out);
    return out;
  };
  const externalize = (id) => ({ id, external: true });
  // Vite's htmlTypeOnLoadCallback: each `<script type="module">` in an html
  // entry becomes an import of its src, and each inline one its own virtual
  // module (variable names may repeat between scripts).
  const htmlScripts = new Map();
  const htmlToJs = (file) => {
    let raw;
    try {
      raw = readFileSync(file, "utf8").replace(HTML_COMMENT_RE, "<!---->");
    } catch {
      return "";
    }
    let js = "";
    let n = 0;
    for (const [, openTag, content] of raw.matchAll(SCRIPT_RE)) {
      const type = TYPE_RE.exec(openTag)
        ?.slice(1)
        .find((v) => v != null);
      if (type !== "module") continue;
      const src = SRC_RE.exec(openTag)
        ?.slice(1)
        .find((v) => v != null);
      if (src) {
        js += `import ${JSON.stringify(src)};\n`;
      } else if (content.trim()) {
        const key = `${HTML_SCRIPT_ID}${file}?id=${n++}`;
        htmlScripts.set(key, content);
        js += `export * from ${JSON.stringify(key)};\n`;
      }
    }
    return `${js}\nexport default {}`;
  };
  const plugin = {
    name: "oj:dep-scan",
    resolveId: {
      async handler(id, importer) {
        if (id === SCAN_INCLUDE_ID || id.startsWith(HTML_SCRIPT_ID)) return id;
        if (!importer) return null;
        // An inline html script imports relative to its html file.
        if (importer.startsWith(HTML_SCRIPT_ID)) importer = cleanUrl(importer.slice(HTML_SCRIPT_ID.length));
        if (EXTERNAL_URL_RE.test(id) || id.startsWith("data:")) return externalize(id);
        if (SPECIAL_QUERY_RE.test(id)) return externalize(id);
        if (BARE_RE.test(id) && !aliasResolve(id)) {
          if (id.includes("?") || moduleListContains(exclude, id) || found.has(id)) return externalize(id);
          // resolve.dedupe from the root here too: the recorded file becomes
          // the bundle ENTRY, so a deduped dep first seen from a linked
          // package must still pin the root copy.
          const from = DEDUPE_PKGS.has(npmPackageName(id)) ? SCAN_INCLUDE_ID : importer;
          const resolved = await resolve(this, id, from);
          if (!resolved || !path.isAbsolute(resolved) || resolved.includes("\0")) return externalize(id);
          // Queries stripped: the recorded file is the bundle ENTRY, a path
          // rolldown must open (and one odd entry would fail the whole build).
          if (isInNodeModules(resolved) || includeIds.includes(id)) {
            if (OPTIMIZABLE_ENTRY_RE.test(cleanUrl(resolved))) found.set(id, cleanUrl(resolved));
            return externalize(id);
          }
          return JS_TYPES_RE.test(cleanUrl(resolved)) ? cleanUrl(resolved) : externalize(id);
        }
        if (EXTERNAL_RE.test(id) || SCAN_EXTERNAL_RE.test(cleanUrl(id))) return externalize(id);
        // `<script src="/main.tsx">`: a dev-server-rooted URL, the file under
        // the root (Vite's resolver maps these the same way).
        if (id.startsWith("/")) {
          const abs = path.join(root, cleanUrl(id));
          if (existsSync(abs)) {
            return JS_TYPES_RE.test(abs) || abs.endsWith(".html") ? abs : externalize(id);
          }
        }
        const resolved = await resolve(this, id, importer);
        if (resolved && path.isAbsolute(resolved) && JS_TYPES_RE.test(cleanUrl(resolved))) return cleanUrl(resolved);
        return externalize(id);
      },
    },
    load: {
      handler(id) {
        if (id === SCAN_INCLUDE_ID) {
          return { code: plainIncludes.map((i) => `import ${JSON.stringify(i)};`).join("\n"), moduleType: "js" };
        }
        if (id.startsWith(HTML_SCRIPT_ID)) return { code: htmlScripts.get(id) ?? "", moduleType: "jsx" };
        if (cleanUrl(id).endsWith(".html")) return { code: htmlToJs(cleanUrl(id)), moduleType: "js" };
        return null;
      },
    },
    // Vite's vite:dep-scan:transform:js-glob, and its html script path: an
    // import per glob-matched file so the crawl follows them. The call itself
    // stays in place; the scan never executes the code.
    transform: {
      filter: { code: /import\.meta\.glob/ },
      handler(code, id) {
        const file = id.startsWith(HTML_SCRIPT_ID) ? cleanUrl(id.slice(HTML_SCRIPT_ID.length)) : cleanUrl(id);
        if (!JS_TYPES_RE.test(file) && !id.startsWith(HTML_SCRIPT_ID)) return null;
        const targets = globImportTargets(code, file);
        if (!targets.length) return null;
        return { code: `${code}\n${targets.map((f) => `import ${JSON.stringify(f)};`).join("\n")}` };
      },
    },
  };
  try {
    await rd.scan({
      ...rolldownInput,
      input,
      cwd: root,
      logLevel: "silent",
      platform: rolldownInput.platform ?? esbuildOptions.platform ?? "browser",
      plugins: [...(host?.plugins ?? []), plugin],
      resolve: { ...rolldownResolveOptions(), ...rolldownInput.resolve },
      transform: { jsx: { runtime: "automatic", development: true }, ...rolldownInput.transform },
      // oj compiles JSX in plain .js app files; the scan must parse them too.
      moduleTypes: { ".js": "jsx", ...rolldownInput.moduleTypes },
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
const CJS_EXTERNAL_FACADE = "\0oj-dep:cjs-external:";
const ASSET_IMPORT_META_URL_RE = /\bnew\s+URL\s*\(\s*('[^']+'|"[^"]+"|`[^`]+`)\s*,\s*import\.meta\.url\s*(?:,\s*)?\)/dg;
const VITE_IGNORE_RE = /\/\*\s*@vite-ignore\s*\*\//;

// strip-literal, small (Vite masks code with it before this match): string,
// comment and regex-literal interiors become spaces, positions kept, so the
// URL pattern never matches inside one. Quotes stay so the pattern still
// matches real code. A `/` opens a regex only where an expression may start.
const REGEX_PRECEDING_WORDS = new Set([
  "return",
  "typeof",
  "instanceof",
  "in",
  "of",
  "new",
  "delete",
  "void",
  "do",
  "else",
  "case",
  "yield",
  "throw",
]);
// After `)`, a regex starts only when the `(` belonged to one of these
// (`if (x) /re/.test(s)` vs the division `f(x) / 2`).
const CONTROL_PAREN_WORDS = new Set(["if", "for", "while", "with"]);
function maskLiterals(code) {
  const out = code.split("");
  const n = code.length;
  const mask = (j) => {
    if (out[j] !== "\n") out[j] = " ";
  };
  // The last non-space, non-masked char and trailing word decide whether a
  // `/` can start a regex (after `(,=:[!&|?{};+-*%<>~^`, a control-flow `)`,
  // or a keyword). `stack` pairs braces/parens so a `}` knows whether it
  // closes a template's `${` (whose tail is a literal again) and a `)` knows
  // its `(`; template interiors are masked, `${expr}` interiors lexed as code.
  let prev = "";
  let word = "";
  let afterControlParen = false;
  let inTemplate = false;
  const stack = [];
  const regexCanStart = () =>
    prev === "" ||
    (prev === ")" ? afterControlParen : "(,=:[!&|?{};+-*%<>~^".includes(prev)) ||
    (word && REGEX_PRECEDING_WORDS.has(word));
  for (let i = 0; i < n;) {
    if (inTemplate) {
      const t = code[i];
      if (t === "\\") {
        mask(i++);
        if (i < n) mask(i++);
      } else if (t === "`") {
        i++;
        inTemplate = false;
        prev = "`";
        word = "";
      } else if (t === "$" && code[i + 1] === "{") {
        i += 2;
        stack.push("template");
        inTemplate = false;
        prev = "{";
        word = "";
      } else {
        mask(i++);
      }
      continue;
    }
    const c = code[i];
    const c2 = code[i + 1];
    if (c === "/" && c2 === "/") {
      while (i < n && code[i] !== "\n") mask(i++);
    } else if (c === "/" && c2 === "*") {
      while (i < n && !(code[i] === "*" && code[i + 1] === "/")) mask(i++);
      if (i < n) mask(i++);
      if (i < n) mask(i++);
    } else if (c === '"' || c === "'") {
      i++;
      while (i < n && code[i] !== c && code[i] !== "\n") {
        if (code[i] === "\\") mask(i++);
        if (i < n) mask(i++);
      }
      i++;
      prev = c;
      word = "";
    } else if (c === "`") {
      i++;
      inTemplate = true;
    } else if (c === "/" && regexCanStart()) {
      mask(i++);
      let inClass = false;
      while (i < n && code[i] !== "\n" && (inClass || code[i] !== "/")) {
        if (code[i] === "\\") mask(i++);
        else if (code[i] === "[") inClass = true;
        else if (code[i] === "]") inClass = false;
        if (i < n) mask(i++);
      }
      if (i < n && code[i] === "/") mask(i++);
      prev = "/";
      word = "";
    } else if (c === "(") {
      stack.push(CONTROL_PAREN_WORDS.has(word) ? "control-paren" : "paren");
      prev = c;
      word = "";
      i++;
    } else if (c === ")") {
      afterControlParen = stack.pop() === "control-paren";
      prev = c;
      word = "";
      i++;
    } else if (c === "{") {
      stack.push("block");
      prev = c;
      word = "";
      i++;
    } else if (c === "}") {
      if (stack.pop() === "template") {
        inTemplate = true;
      } else {
        prev = c;
        word = "";
      }
      i++;
    } else {
      if (/[A-Za-z0-9_$]/.test(c)) word = /[A-Za-z0-9_$]/.test(code[i - 1] ?? "") ? word + c : c;
      if (!/\s/.test(c)) prev = c;
      i++;
    }
  }
  return out.join("");
}

/// Vite's rolldownDepPlugin, with one difference: oj serves the bundle
/// verbatim (Vite runs importAnalysis over it), so whatever stays external
/// is written as the URL oj serves it at instead of a path or bare name.
function rolldownDepPlugins() {
  const rootImporter = path.join(root, "package.json");
  const rootDirs = new Set([root, realRoot]);
  // Vite's resolveResult: a bare import can RESOLVE to a non-JS file (a
  // css-main package); that file is externalized like an asset, not bundled.
  const assetOrModule = (r, kind) => {
    if (!r || r.external || !EXTERNAL_RE.test(r.id)) return r;
    const url = urlOf(r.id);
    return kind === "require-call" ? { id: CONVERT_EXTERNAL + url } : { id: url, external: "absolute" };
  };
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
        async handler(id, importer, opts) {
          if (id.startsWith(CONVERTED_PREFIX)) return { id: id.slice(CONVERTED_PREFIX.length), external: "absolute" };
          if (!importer) return null;
          if (moduleListContains(exclude, id)) {
            const r = await this.resolve(id, importer, { skipSelf: true });
            const url = r && !r.external ? urlOf(r.id) : id;
            // An ESM bundle has no require(): Vite's cjs-external plugin turns
            // a require() of an excluded dep into an import through a facade.
            if (opts?.kind === "require-call") return { id: CJS_EXTERNAL_FACADE + url };
            return { id: url, external: "absolute" };
          }
          const aliased = aliasResolve(id);
          if (aliased) {
            const r = await this.resolve(aliased, importer, { skipSelf: true });
            if (r) return assetOrModule(r, opts?.kind);
          }
          // resolve.dedupe: resolve from the project root wherever the
          // importer sits (Vite resolve.ts: dedupe -> basedir = root).
          const fromRoot = DEDUPE_PKGS.has(npmPackageName(id)) && !rootDirs.has(path.dirname(importer));
          const r = await this.resolve(id, fromRoot ? rootImporter : importer, { skipSelf: true });
          if (r) return assetOrModule(r, opts?.kind);
          if (NODE_BUILTINS.has(id)) return { id: BROWSER_EXTERNAL + id };
          const peer = optionalPeerOf(id, importer);
          if (peer) return { id: OPTIONAL_PEER + peer };
          return null;
        },
      },
      load: {
        filter: { id: /^\0oj-dep:(browser-external|optional-peer|cjs-external):/ },
        handler(id) {
          if (id.startsWith(CJS_EXTERNAL_FACADE)) {
            const spec = JSON.stringify(CONVERTED_PREFIX + id.slice(CJS_EXTERNAL_FACADE.length));
            return `import * as m from ${spec};\nmodule.exports = { ...m };\n`;
          }
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
      // serves that file at (Vite rewrites it relative to its deps dir). The
      // match runs over literal-masked code, as Vite's does over strip-literal,
      // so the pattern inside a string or comment is left alone, and
      // `/* @vite-ignore */` skips a site.
      transform: {
        filter: { code: /import\.meta\.url/ },
        handler(code, id) {
          const masked = maskLiterals(code);
          let out = "";
          let last = 0;
          ASSET_IMPORT_META_URL_RE.lastIndex = 0;
          for (let m; (m = ASSET_IMPORT_META_URL_RE.exec(masked));) {
            const [start, end] = m.indices[0];
            const [rawStart, rawEnd] = m.indices[1];
            const raw = code.slice(rawStart, rawEnd);
            if (VITE_IGNORE_RE.test(code.slice(start, rawStart))) continue;
            if (raw[0] === "`" && raw.includes("${")) continue;
            const url = raw.slice(1, -1);
            if (url.startsWith("data:") || url.startsWith("/") || EXTERNAL_URL_RE.test(url)) continue;
            out += code.slice(last, start);
            out += `new URL('' + ${JSON.stringify(urlOf(path.resolve(path.dirname(id), url)))}, import.meta.url)`;
            last = end;
          }
          return last ? { code: out + code.slice(last) } : null;
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
/// Vite's extractExportsData fallback: a dep entry that only parses as JSX
/// (JSX shipped in a .js file) switches the whole bundle to `.js: "jsx"`.
function needsJsxLoader(parseSync, entryPoints) {
  if (!parseSync) return false;
  return Object.values(entryPoints).some((file) => {
    if (!file.endsWith(".js")) return false;
    let code;
    try {
      code = readFileSync(file, "utf8");
    } catch {
      return false;
    }
    const failed = (lang) => {
      try {
        return parseSync(file, code, { lang }).errors.length > 0;
      } catch {
        return true;
      }
    };
    return failed("js") && !failed("jsx");
  });
}

async function rolldownBundle(rd, entryPoints) {
  const jsxLoader = needsJsxLoader(rd.parseSync, entryPoints);
  // `.css: "js"` is Vite's guard (prepareRolldownOptimizerRun): CSS is
  // externalized at resolve, so none should load; one that slips through must
  // not wake rolldown's own CSS handling, whose output nothing here serves.
  const moduleTypes = { ".css": "js", ...rolldownInput.moduleTypes, ...(jsxLoader ? { ".js": "jsx" } : {}) };
  const bundle = await rd.rolldown({
    ...rolldownInput,
    input: entryPoints,
    cwd: root,
    logLevel: "silent",
    platform: rolldownInput.platform ?? esbuildOptions.platform ?? "browser",
    plugins: rolldownDepPlugins(),
    ...(esbuildOptions.external ? { external: esbuildOptions.external } : {}),
    transform: {
      target: esbuildOptions.target ?? BASELINE_TARGET,
      ...rolldownInput.transform,
      define: {
        "process.env.NODE_ENV": JSON.stringify(nodeEnv),
        ...(esbuildOptions.define ?? {}),
        ...rolldownInput.transform?.define,
      },
    },
    resolve: { ...rolldownResolveOptions(), ...rolldownInput.resolve },
    moduleTypes,
  });
  let output;
  try {
    ({ output } = await bundle.write({
      ...rolldownOutput,
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

/// Runs the dep pre-bundle and returns `{ metadata }`. Called by oj
/// through a short-lived JS engine; the config arrives as a JSON argument and
/// the metadata leaves as the return value (no argv, no stdout), so neither an
/// oversized include list nor a dep that prints on require can break the
/// channel.
function configure(input) {
  ({
    root,
    outDir,
    entries,
    include = [],
    exclude = [],
    dedupe = [],
    alias = [],
    autoDiscover = true,
    nodeEnv = "development",
    buildInputs = [],
    buildOutDir = "dist",
    resolve: resolveSettings = {},
  } = input);
  const { plugins: _plugins, output = {}, ...rest } = input.rolldownOptions ?? {};
  rolldownInput = rest;
  rolldownOutput = output;
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
  // The user's list warns when an entry does not resolve; oj's implicit
  // includes (the injected JSX runtime) only bundle when present.
  warnIncludes = new Set(includeIds);
  for (const inc of input.implicitInclude ?? []) {
    if (!includeIds.includes(inc)) includeIds.push(inc);
  }
  // optimizeDeps.entries are glob patterns relative to root (Vite scans them with
  // tinyglobby); a literal path is used as given. Like Vite's isScannable +
  // exists filter, only JS and html files feed the scan.
  entryList = (
    entries && entries.length
      ? entries.flatMap((e) =>
          isDynamicPattern(e)
            ? globFiles(e, root)
                .filter((f) => !f.split("/").includes("node_modules"))
                .map((f) => path.join(root, f))
            : [path.isAbsolute(e) ? e : path.join(root, e)],
        )
      : detectEntries()
  ).filter((f) => (JS_TYPES_RE.test(f) || f.endsWith(".html")) && existsSync(f));
  DEDUPE_PKGS = new Set(dedupe.map(npmPackageName).filter(Boolean));
  aliasEntries = [
    ...loadTsconfigAliases(root),
    ...(alias || []).map(([find, replacement]) => ({ exact: find, prefix: find + "/", target: replacement })),
  ];
}

/// The dep scan alone, run inside the plugin host so every import resolves
/// through the app's plugins first (Vite's scan goes through
/// `pluginContainer.resolveId(..., { scan: true })`) and the config's live
/// `optimizeDeps.rolldownOptions.plugins` take part. Only the scan runs there:
/// a dep bundle in that long-lived isolate keeps hundreds of MB resident. The
/// result feeds `optimize()` as `scanned`.
export async function scan(input, host) {
  configure(input);
  const rd = await loadRolldown(pickRolldown(root, input.vendoredRolldown));
  return Object.fromEntries(await rolldownScan(rd, autoDiscover, host));
}

export async function optimize(input) {
  configure(input);
  const rd = await loadRolldown(pickRolldown(root, input.vendoredRolldown));
  const candidates = input.scanned ? new Map(Object.entries(input.scanned)) : await rolldownScan(rd, autoDiscover);
  // "a > b" names a nested copy the scan cannot resolve (it resolves below),
  // and a plain include the scan could not resolve must reach the loop too,
  // so its unableToOptimize warning fires instead of a silent drop.
  for (const inc of includeIds) if (!candidates.has(inc)) candidates.set(inc, null);
  const deps = [...candidates.keys()].filter((d) => !excludeSet.has(d));

  const entryPoints = {};
  const nameOf = {};
  // Vite's unableToOptimize: a dep named in `optimizeDeps.include` that does
  // not resolve is dropped with a warning, never silently.
  const unableToOptimize = (dep) => {
    if (warnIncludes.has(dep) || dep.includes(">")) {
      console.warn(`oj: failed to resolve dependency "${dep}", present in 'optimizeDeps.include'`);
    }
  };
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
        unableToOptimize(dep);
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
    // Entries are resolved by the scan with the bundle's own resolver; one
    // that did not resolve there would fail the whole build.
    const entry = candidates.get(dep);
    if (!entry) {
      unableToOptimize(dep);
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
    // The file the scan's browser resolver picked (Vite's flatIdDeps), never a
    // `req.resolve` path, which picks the `require`/`node` condition.
    entryPoints[name] = entry;
    nameOf[dep] = name;
  }

  rmSync(outDir, { recursive: true, force: true });
  mkdirSync(outDir, { recursive: true });

  const metadata = {};
  if (Object.keys(entryPoints).length) {
    const built = await rolldownBundle(rd, entryPoints);
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

  return { metadata };
}
