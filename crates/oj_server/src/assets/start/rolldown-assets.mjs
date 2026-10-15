// SPDX-License-Identifier: MIT

import { readFileSync, readdirSync, existsSync, mkdirSync, writeFileSync } from "node:fs";
import { builtinModules } from "node:module";
import { join, dirname, extname, basename, resolve, relative, isAbsolute, sep } from "node:path";
import { createHash } from "node:crypto";
import { emptyVirtualStub } from "./resolve-pkg.mjs";

// Vite's import-query regexes (utils.ts urlRE/rawRE, asset.ts inlineRE,
// worker.ts workerOrSharedWorkerRE): a query matches in any position and
// combination, e.g. `?worker&url`, `?url&no-inline`.
const URL_RE = /(\?|&)url(?:&|$)/;
const RAW_RE = /(\?|&)raw(?:&|$)/;
const INLINE_RE = /[?&]inline\b/;
const NO_INLINE_RE = /(\?|&)no-inline(?:&|$)/;
const WORKER_RE = /(?:\?|&)(worker|sharedworker)(?:&|$)/;
const ASSET_EXT = /\.(png|jpe?g|gif|webp|avif|ico|woff2?|ttf|otf|eot|mp4|webm|wasm)(\?|$)/;
// Vite's CSS_LANGS_RE and cssModuleRE over the languages oj compiles.
const STYLE_RE = /\.(css|less|sass|scss|styl|stylus)(\?|$)/;
const CSS_MODULE_RE = /\.module\.(css|less|sass|scss|styl|stylus)$/;
const STYLE_SCHEME = "oj-style:";

// esbuild "namespaces" become \0-prefixed virtual ids; the tag routes load().
const V = (tag, path) => `\0oj-${tag}:${path}`;
const parseV = (id) => {
  if (!id.startsWith("\0oj-")) return null;
  const i = id.indexOf(":");
  return { tag: id.slice(4, i), path: id.slice(i + 1) };
};

const dataUri = (abs) => {
  const buf = readFileSync(abs);
  const mime =
    {
      ".png": "image/png",
      ".jpg": "image/jpeg",
      ".jpeg": "image/jpeg",
      ".gif": "image/gif",
      ".webp": "image/webp",
      ".avif": "image/avif",
      ".svg": "image/svg+xml",
      ".ico": "image/x-icon",
      ".woff": "font/woff",
      ".woff2": "font/woff2",
      ".ttf": "font/ttf",
      ".otf": "font/otf",
    }[extname(abs).toLowerCase()] || "application/octet-stream";
  return `data:${mime};base64,${buf.toString("base64")}`;
};

const makeUrlFor =
  ({ mode, fsBase, emit }) =>
  async (abs) =>
    mode === "dev" ? fsBase + abs : emit(abs);

// A worker's URL, Vite's worker plugin load. Unbundled dev: the file's URL on
// the dev pipeline (fileToUrl: root-relative, `/@fs` outside the root), which
// serves the compiled module worker and is what the SSR host renders. Vite
// additionally tags this URL `?worker_file&type=module` for its own serving
// pipeline; oj's pipeline serves the compiled module without the marker
// (verified in-browser by e2e/start-ssr-worker-url.mjs). A build: the worker
// entry bundled on its own (bundleWorkerEntry) and emitted once.
const makeWorkerUrlFor =
  ({ mode, root, emit }) =>
  async (abs) => {
    if (mode !== "dev") return emit.worker(abs);
    const rel = root ? relative(root, abs) : null;
    return rel && !rel.startsWith("..") && !isAbsolute(rel) ? "/" + rel.split(sep).join("/") : "/@fs" + abs;
  };

// Vite's `?worker&inline` in a bundled environment (worker.ts load): the
// bundled worker ships inside the importer as a string and starts from a Blob
// URL, falling back to a data: URL; a SharedWorker always uses the data: URL
// (a blob URL would make separate instances). Module workers (format es).
function inlineWorkerModule(ctor, entryCode) {
  const jsContent = `const jsContent = ${JSON.stringify(entryCode)};`;
  const typeOption = `{ type: "module", name: options?.name }`;
  if (ctor === "Worker") {
    return `${jsContent}
const blob = typeof self !== "undefined" && self.Blob && new Blob(['URL.revokeObjectURL(import.meta.url);', jsContent], { type: "text/javascript;charset=utf-8" });
export default function WorkerWrapper(options) {
  let objURL;
  try {
    objURL = blob && (self.URL || self.webkitURL).createObjectURL(blob);
    if (!objURL) throw '';
    const worker = new Worker(objURL, ${typeOption});
    worker.addEventListener("error", () => {
      (self.URL || self.webkitURL).revokeObjectURL(objURL);
    });
    return worker;
  } catch (e) {
    return new Worker('data:text/javascript;charset=utf-8,' + encodeURIComponent(jsContent), ${typeOption});
  }
}
`;
  }
  return `${jsContent}
export default function WorkerWrapper(options) {
  return new ${ctor}('data:text/javascript;charset=utf-8,' + encodeURIComponent(jsContent), ${typeOption});
}
`;
}

// oj's asset plugin resolving the file behind an asset import (`x.css` for
// `x.css?url`): in flight, keyed by specifier and importer, so the app-plugin
// routing can leave it alone. RESIDUAL RACE, accepted: a real `./x.css`
// import from the same importer resolving concurrently with the sibling
// `./x.css?url`'s inner resolve is indistinguishable by value (rolldown does
// not reliably deliver `custom` across plugins, so causality is invisible)
// and skips the app-plugin routing for that one resolution -- which is
// exactly the pre-routing behavior, never worse; the asset still serves.
const assetInnerResolves = new Map();
const assetInnerKey = (source, importer) => `${source}\0${importer ?? ""}`;

// The oj asset module a specifier (or a resolved `/abs/x.css?url`) maps to, by
// its query and extension; null when it is not an asset import.
function assetTagFor(source, mode) {
  const worker = WORKER_RE.exec(source);
  // Vite's worker load: a bundled environment checks `&inline` before `&url`;
  // unbundled dev ignores `&inline`.
  if (worker)
    return worker[1] + (mode !== "dev" && INLINE_RE.test(source) ? "-inline" : URL_RE.test(source) ? "-url" : "");
  if (RAW_RE.test(source)) return "raw";
  // Vite's shouldInline: `no-inline` wins, then `inline` (even with `url`),
  // the order the SSR host's split_asset_query uses, so both sides agree.
  if (NO_INLINE_RE.test(source)) return "url";
  if (INLINE_RE.test(source)) return "inline";
  if (URL_RE.test(source)) return "url";
  if (ASSET_EXT.test(source)) return "url";
  if (STYLE_RE.test(source)) return "css";
  return null;
}

// `styleKey`: query params a long-lived engine adds to its style imports,
// which it caches by url: `css` moves when a style input changed, `inline`
// every bundle (a Tailwind sheet reads every source file).
export function assetsPlugin({
  mode = "dev",
  server = false,
  fsBase = "/@oj-start/fs",
  emit,
  cssUrls,
  root,
  styleKey = {},
} = {}) {
  const urlFor = makeUrlFor({ mode, fsBase, emit });
  const workerUrlFor = makeWorkerUrlFor({ mode, root, emit });
  return {
    name: "oj-assets",
    resolveId: {
      filter: { id: { include: [WORKER_RE, URL_RE, RAW_RE, INLINE_RE, NO_INLINE_RE, ASSET_EXT, STYLE_RE] } },
      async handler(source, importer, options) {
        if (options?.custom?.ojAsset) return null;
        const tag = assetTagFor(source, mode);
        if (!tag) return null;
        const clean = source.replace(/\?.*$/, "");
        // Marked in-flight rather than only by `custom`: rolldown does not
        // reliably hand `custom` to the hooks this resolve reaches (observed:
        // the routing plugin saw it without), and the app plugins must stay out.
        const key = assetInnerKey(clean, importer);
        assetInnerResolves.set(key, (assetInnerResolves.get(key) ?? 0) + 1);
        let r;
        try {
          r = await this.resolve(clean, importer, { skipSelf: true, custom: { ojAsset: true } });
        } finally {
          const left = assetInnerResolves.get(key) - 1;
          if (left > 0) assetInnerResolves.set(key, left);
          else assetInnerResolves.delete(key);
        }
        if (!r) return null;
        // The file may come back as an oj module id: a concurrent resolution of
        // the same file (`import "./x.css"` next to this `./x.css?url`) can
        // answer this one. Its file is what this import's tag wraps.
        const v = parseV(r.id);
        const file = v ? v.path.replace(/\?.*$/, "") : r.id;
        return V(tag, file);
      },
    },
    load: {
      filter: { id: /^\0oj-(raw|url|inline|css|(?:shared)?worker(?:-url|-inline)?):/ },
      async handler(id) {
        const v = parseV(id);
        if (!v) return null;
        const js = (code) => ({ code, moduleType: "js" });
        if (v.tag === "raw") return js(`export default ${JSON.stringify(readFileSync(v.path, "utf8"))};`);
        if (v.tag === "url") return js(`export default ${JSON.stringify(await urlFor(v.path))};`);
        if (v.tag === "inline" && STYLE_RE.test(v.path)) {
          // Vite's `?inline` sheet: the compiled stylesheet as a string
          // (css.ts), never linked. Dev takes the module oj's dev server
          // renders for SSR; the build compiles it as it emits sheets.
          const style = (extra) => import(STYLE_SCHEME + "///?" + new URLSearchParams({ file: v.path, ...extra }));
          if (mode === "dev") return js((await style({ inline: "dev", ...styleKey.inline })).code);
          const compiled = await style({});
          // A build gives a CSS module's class map even inlined (css.ts:
          // modulesCode wins over `inlined` there, not in dev).
          if (CSS_MODULE_RE.test(v.path)) return js(compiled.code);
          const css = await emit.inlineCss(v.path, compiled.css, server);
          return js(`export default ${JSON.stringify(css)};`);
        }
        if (v.tag === "inline") return js(`export default ${JSON.stringify(dataUri(v.path))};`);
        if (v.tag === "worker-url" || v.tag === "sharedworker-url") {
          return js(`export default ${JSON.stringify(await workerUrlFor(v.path))};`);
        }
        if (v.tag === "worker-inline" || v.tag === "sharedworker-inline") {
          return js(
            inlineWorkerModule(
              v.tag === "sharedworker-inline" ? "SharedWorker" : "Worker",
              await emit.workerCode(v.path),
            ),
          );
        }
        if (v.tag === "worker" || v.tag === "sharedworker") {
          const ctor = v.tag === "sharedworker" ? "SharedWorker" : "Worker";
          const url = JSON.stringify(await workerUrlFor(v.path));
          return js(
            `export default function WorkerWrapper(options) { return new ${ctor}(${url}, { type: "module", name: options?.name }); }`,
          );
        }
        if (v.tag === "css") {
          // A CSS module or a preprocessor file compiles on oj's CSS pipeline
          // (the module host's STYLE_SCHEME), the compile the dev server's SSR
          // renders from: preprocessor first, then CSS-module scoping, as in
          // Vite's compileCSS. So class names agree across SSR, the client
          // and the build. The build emits that sheet; dev serves the same one
          // by url. A plain .css file is linked as written.
          const compiled =
            CSS_MODULE_RE.test(v.path) || !v.path.endsWith(".css")
              ? await import(STYLE_SCHEME + "///?" + new URLSearchParams({ file: v.path, ...styleKey.css }))
              : null;
          if (!server) {
            const href = compiled && mode !== "dev" ? await emit.css(v.path, compiled.css) : await urlFor(v.path);
            if (cssUrls && !cssUrls.includes(href)) cssUrls.push(href);
          }
          return js(compiled ? compiled.code : "export default {};");
        }
        return null;
      },
    },
  };
}

// A plugin resolveId answer as a bundler id, Rollup's contract: external
// stays external, an absolute path is that file, loaded like any other (a
// missing one fails loudly, as in Vite). A path with a query (`/a/x.css?url`)
// is what Vite's load hooks key on (vite:asset, vite:worker), as is a
// stylesheet or asset path (vite:css); oj answers those with its asset module
// for the same id (the path is already resolved, so no second resolve). Any
// other id with a query, and a virtual id, is served by the plugins' load.
function bundlerIdFor(r, mode) {
  if (r.external) return { id: r.id, external: true };
  // oj's own module ids (an asset or worker module the bundler resolved and a
  // plugin handed back from its this.resolve) are already bundler ids.
  if (parseV(r.id)) return r.id;
  const file = r.id.replace(/\?.*$/, "");
  if (!r.id.startsWith("\0") && isAbsolute(file)) {
    const tag = assetTagFor(r.id, mode);
    if (tag) return V(tag, file);
    if (!r.id.includes("?")) return r.id;
    // A real source file with a foreign query (`/abs/route.ts?tsr-split=x`)
    // stays a PIPELINE id: transform and load filters key on it (Vite's
    // contract), and the load below serves it (the plugin's load on the full
    // id, then the clean file from disk). Only ids the pipeline's load filter
    // cannot reach (node_modules, exotic extensions) go through vite-virtual.
    if (!file.includes("/node_modules/") && /\.(jsx?|mjs|tsx?)$/.test(file)) return r.id;
  }
  return V("vite-virtual", r.id);
}
// Specifiers oj's asset plugin owns. Vite's core resolver answers them (the
// file exists), so a normal plugin's resolveId never sees them; the post
// phase leaves them to the asset plugin, which also keeps its own nested
// this.resolve out of the core-resolve round trip below.
const ASSET_OWNED = [WORKER_RE, URL_RE, RAW_RE, INLINE_RE, NO_INLINE_RE, ASSET_EXT, STYLE_RE];
// A bare specifier: not relative, absolute, a \0 id or a `scheme:` URL.
const BARE_RE = /^(?![./\\\0]|[a-zA-Z]:[\\/]|[a-zA-Z][\w+.-]*:)/;
const matchesAny = (res, id) =>
  res.some((re) => {
    const hit = re.test(id);
    re.lastIndex = 0;
    return hit;
  });

// Vite's normalizeAlias (utils.ts): an object or array `resolve.alias` as an
// ordered entry list, a trailing `/` dropped from both sides when both carry it.
function normalizeAlias(alias) {
  const entries = Array.isArray(alias)
    ? alias
    : Object.entries(alias ?? {}).map(([find, replacement]) => ({ find, replacement }));
  return entries
    .filter((e) => e && typeof e.replacement === "string" && (typeof e.find === "string" || e.find instanceof RegExp))
    .map(({ find, replacement }) =>
      typeof find === "string" && find.endsWith("/") && replacement.endsWith("/")
        ? { find: find.slice(0, -1), replacement: replacement.slice(0, -1) }
        : { find, replacement },
    );
}

// @rollup/plugin-alias's matcher: a string matches itself or itself plus `/…`.
function aliasMatches(find, id) {
  if (find instanceof RegExp) {
    find.lastIndex = 0;
    return find.test(id);
  }
  return id === find || id.startsWith(`${find}/`);
}

// The app's `resolve.alias`, as Vite applies it (plugins/index.ts): the
// alias plugin runs ahead of every app plugin and the core resolver, the
// first matching entry rewrites the specifier, and the rewritten id resolves
// through the rest of the pipeline. Without it an `@/x` import resolves only
// where a tsconfig-paths plugin happens to cover the importer. A rewritten id
// nothing resolves stays the answer, as in rolldown's viteAliasPlugin and
// Vite's viteAliasCustomResolver: the import then fails to load, loudly,
// instead of reaching later resolvers as the original bare `@/x` and
// shipping to the browser unresolved.
export function appAliasPlugin(config) {
  const entries = normalizeAlias(config?.resolve?.alias);
  if (!entries.length) return null;
  return {
    name: "oj-app-alias",
    async resolveId(source, importer, options) {
      const entry = entries.find((e) => aliasMatches(e.find, source));
      if (!entry) return null;
      const updated = source.replace(entry.find, entry.replacement);
      const resolved = await this.resolve(updated, importer, { ...options, skipSelf: true });
      if (resolved) return resolved;
      if (!isAbsolute(updated)) {
        this.warn(
          `rewrote ${source} to ${updated} but was not an absolute path and was not handled by other plugins. ` +
            `This will lead to duplicated modules for the same path. ` +
            `To avoid duplicating modules, you should resolve to an absolute path.`,
        );
      }
      return { id: updated };
    },
  };
}

const stripAnsi = (s) => String(s ?? "").replace(/\x1b\[[0-9;]*m/g, "");

// Vite's generateCodeFrame layout (utils.ts) for one position: two lines of
// context either side and a caret under the column. Null when the file cannot
// be read.
function sourceFrame(loc) {
  if (!loc?.file || !loc.line) return null;
  let source;
  try {
    source = readFileSync(loc.file, "utf8");
  } catch {
    return null;
  }
  const lines = source.split(/\r?\n/);
  const at = loc.line - 1;
  const width = Math.max(3, String(Math.min(lines.length, loc.line + 2)).length + 1);
  const out = [];
  for (let i = Math.max(0, at - 2); i <= Math.min(lines.length - 1, at + 2); i++) {
    const n = String(i + 1);
    out.push(`${" ".repeat(width - n.length)}${n}|  ${lines[i]}`);
    if (i === at) out.push(`${" ".repeat(width)}|  ${" ".repeat(loc.column ?? 0)}^`);
  }
  return out.join("\n");
}

// Vite's onRollupLog (build.ts), shared by its bundled dev engine and the
// build: an import nothing resolved throws, where rolldown's default warns
// and leaves it external. A typo or an unaliased `@/x` then fails the bundle
// with the file and position instead of shipping a bare specifier the browser
// cannot load (a blank page). `?commonjs-external` ids stay exempt, as there.
export function viteOnLog(level, log, defaultHandler) {
  if (log.code === "UNRESOLVED_IMPORT" && !log.id?.endsWith("?commonjs-external")) {
    const err = new Error(
      `[vite]: Rolldown failed to resolve import "${log.exporter}" from "${log.id}".\n` +
        `This is most likely unintended because it can break your application at runtime.\n` +
        `If you do want to externalize this module explicitly add it to\n` +
        "`build.rolldownOptions.external`",
    );
    // The code frame from the source (rolldown's own frame says it is
    // "treating it as an external dependency", which this error is not), and
    // no stack: this throw site is oj's, not the app's.
    Object.assign(err, { id: log.id, loc: log.loc, frame: sourceFrame(log.loc) ?? stripAnsi(log.message), stack: "" });
    throw err;
  }
  defaultHandler(level, log);
}

// The app's own `external` for a client bundle, which Vite's
// resolveRolldownOptions passes to bundled dev and the build alike: an import
// the app externalizes on purpose is never an unresolved one.
export function clientExternal(config) {
  const env = config?.environments?.client?.build?.rolldownOptions?.external;
  const top = config?.build?.rolldownOptions?.external ?? config?.build?.rollupOptions?.external;
  const external = env ?? top;
  return external === undefined ? {} : { external };
}

// Vite's prepareError (middlewares/error.ts): the ErrorPayload `err` its
// clients render. A rolldown build failure aggregates its causes under
// `errors`; the first one carries the file, position and frame.
export function buildErrorPayload(error) {
  const e = error?.errors?.[0] ?? error;
  return {
    message: stripAnsi(e?.message ?? e),
    stack: stripAnsi(e?.stack ?? ""),
    id: e?.id,
    frame: stripAnsi(e?.frame ?? ""),
    plugin: e?.plugin,
    pluginCode: e?.pluginCode?.toString(),
    loc: e?.loc,
  };
}

// `lifecycle: false` for a nested build (a worker bundle): the container's
// buildStart/buildEnd/renderStart belong to the build that owns it, not to a
// bundle it triggers mid-build (Vite's bundleWorkerEntry runs its own worker
// plugin instances, never the importer's lifecycle).
export function makeVitePlugins({
  container,
  fallback,
  appRoot,
  mode = "dev",
  fsBase = "/@oj-start/fs",
  emit,
  lifecycle = true,
  // False when the container outlives this build (the dev client bundle):
  // its buildEnd belongs to the session's end.
  buildEnd = lifecycle,
} = {}) {
  const urlFor = makeUrlFor({ mode, fsBase, emit });
  const warnedVirtual = new Set();
  // The app plugins' resolveId, in Vite's order around its core resolver: a
  // pre-phase hook first, the core resolver next, every other hook only for
  // an id the core resolver left. Routed on declared filters, the dev
  // server's gate (resolve_id_res): a hook with no filter is not offered
  // every import (it still answers `virtual:` / `\0` ids above), except that
  // a bare specifier is offered to it, in its own phase, as the dev server's
  // plugin fallback does. The rolldown filter widens by exactly that.
  const plan = container?.resolveIdPlan?.() ?? { pre: { all: false, filters: [] }, post: { all: false, filters: [] } };
  // An unfiltered hook in EITHER phase gets the bare-specifier fallback (the
  // dev server's bounded-cost gate): most ecosystem plugins declare no
  // filter, and an unfiltered enforce:"pre" plugin must not silently resolve
  // in the SSR dev server but never in this bundle.
  const userIncludes = [...plan.pre.filters, ...plan.post.filters, ...(plan.pre.all || plan.post.all ? [BARE_RE] : [])];
  const svgModule = async (path, id) => {
    if (container) {
      const code = await container.load(id);
      if (code != null) return { code, moduleType: "jsx" };
    }
    return { code: `export default ${JSON.stringify(await urlFor(path))};`, moduleType: "js" };
  };
  return {
    name: "oj-vite-plugins",
    async buildStart() {
      if (!lifecycle) return;
      // Run user plugins' buildStart before any module loads, so compile-on-
      // startup plugins (e.g. i18n) have populated the state their load() serves.
      if (container?.buildStart) await container.buildStart();
      if (fallback?.buildStart && fallback !== container) await fallback.buildStart();
    },
    async buildEnd(error) {
      if (lifecycle && buildEnd && container?.buildEnd) await container.buildEnd(error);
    },
    async renderStart(outputOptions, inputOptions) {
      if (lifecycle && container?.renderStart) await container.renderStart(outputOptions, inputOptions);
    },
    resolveId: {
      filter: { id: { include: [/\.svg\?react$/, /^virtual:/, /^\0/, ...userIncludes] } },
      async handler(source, importer, options) {
        if (options?.custom?.ojSvg || options?.custom?.ojCoreResolve) return null;
        if (/\.svg\?react$/.test(source)) {
          const r = await this.resolve(source.slice(0, -"?react".length), importer, {
            skipSelf: true,
            custom: { ojSvg: true },
          });
          return r ? V("svg-react", r.id) : null;
        }
        if (!container) return null;
        if (/^virtual:/.test(source) || source.startsWith("\0")) {
          if (parseV(source)) return null;
          const rid = await container.resolveId(source, importer);
          return rid ? V("vite-virtual", rid) : null;
        }
        // oj's asset plugin resolving the file behind `x.css?url` is not an
        // import: Vite never resolves the bare file separately, so the app
        // plugins (which saw the full specifier) stay out of it.
        if (options?.custom?.ojAsset || assetInnerResolves.has(assetInnerKey(source, importer))) return null;
        // A plugin's own this.resolve ends in this bundler's resolution.
        const bundlerResolve = async (s, i) => {
          const r = await this.resolve(s, i, { skipSelf: true, custom: { ojCoreResolve: true } });
          return r ? { id: r.id, external: r.external } : null;
        };
        if (matchesAny(plan.pre.filters, source) || (plan.pre.all && BARE_RE.test(source))) {
          const r = await container.resolveIdResult(source, importer, null, "pre", bundlerResolve);
          if (r) return bundlerIdFor(r, mode);
        }
        if (
          !matchesAny(ASSET_OWNED, source) &&
          (matchesAny(plan.post.filters, source) || (plan.post.all && BARE_RE.test(source)))
        ) {
          const core = await this.resolve(source, importer, {
            ...options,
            skipSelf: true,
            custom: { ...options?.custom, ojCoreResolve: true },
          });
          if (core) return core;
          const r = await container.resolveIdResult(source, importer, null, "post", bundlerResolve);
          if (r) return bundlerIdFor(r, mode);
        }
        return null;
      },
    },
    load: {
      filter: { id: { include: [/^\0oj-/, /\.svg$/, /^(?!.*\/node_modules\/).*\.(jsx?|mjs|tsx?)(\?|$)/] } },
      async handler(id) {
        const v = parseV(id);
        if (v && v.tag === "svg-react") return svgModule(v.path, v.path + "?react");
        if (v && v.tag === "vite-virtual") {
          let code = await container.load(v.path);
          if (code == null && fallback) code = await fallback.load(v.path);
          // Rollup's contract: a resolved id no load() claims is read from
          // disk (`/abs/x.ts?q` from a plugin's resolveId).
          const file = v.path.startsWith("\0") ? null : v.path.replace(/\?.*$/, "");
          const ext = file ? extname(file).slice(1) : "";
          // Disk fallback parses as the FILE's own language (.mts/.cts are
          // TS, never JSX); plugin-loaded code keeps a tolerant superset --
          // tsx when the id is TS-family (types AND jsx parse), jsx otherwise
          // (the pre-existing default: a load() may return JSX for any id).
          const diskType =
            { mts: "ts", cts: "ts", ts: "ts", tsx: "tsx", jsx: "jsx", mjs: "js", cjs: "js", js: "js" }[ext] ?? "js";
          const typed = ["ts", "mts", "cts", "tsx"].includes(ext) ? "tsx" : "jsx";
          if (code == null && file && isAbsolute(file) && existsSync(file)) {
            return { code: readFileSync(file, "utf8"), moduleType: diskType };
          }
          if (code == null) {
            if (!warnedVirtual.has(v.path)) {
              warnedVirtual.add(v.path);
              process.stderr.write(
                `oj: plugin virtual "${v.path}" produced no content in the dev client bundle; ` +
                  `emitting an empty module. This virtual likely needs the full build graph oj does not run in dev.\n`,
              );
            }
            return { code: emptyVirtualStub(appRoot, v.path), moduleType: "js" };
          }
          return { code, moduleType: typed };
        }
        if (/\.svg$/.test(id) && !id.startsWith("\0")) return svgModule(id, id);
        // A user plugin's load() may override a real on-disk source file (Vite:
        // load runs before the fs read). Consult it for user files in the build
        // too, so compile-on-startup plugins produce the same output as dev.
        if (container && !id.startsWith("\0") && !id.includes("/node_modules/")) {
          const cleanId = id.replace(/\?.*$/, "");
          if (/\.(jsx?|mjs|tsx?)$/.test(cleanId)) {
            // The FULL id first: a plugin's load keys on its own query
            // (`?tsr-split=...`), Vite's contract; then the clean file.
            let code = null;
            for (const c of [container, fallback]) {
              if (!c) continue;
              if (id !== cleanId) code = await c.load(id);
              if (code == null) code = await c.load(cleanId);
              if (code != null) break;
            }
            if (code == null && id !== cleanId && existsSync(cleanId)) {
              // Rollup's contract: a resolved id no load() claims reads the
              // file behind the query from disk.
              code = readFileSync(cleanId, "utf8");
            }
            if (code != null) {
              const moduleType = cleanId.endsWith(".tsx")
                ? "tsx"
                : cleanId.endsWith(".ts")
                  ? "ts"
                  : cleanId.endsWith(".jsx")
                    ? "jsx"
                    : "js";
              return { code, moduleType };
            }
          }
        }
        return null;
      },
    },
    transform: {
      filter: { id: /\.mdx?$/ },
      async handler(code, id) {
        if (!container) return null;
        if (/\.mdx?$/.test(id)) {
          const out = await container.transform(code, id);
          return out == null ? null : out;
        }
        return null;
      },
    },
  };
}

// Every shim is CommonJS, matching Vite's browser-external stubs: named ESM
// imports from a CJS module interop into property reads, so a name the shim
// does not carry is undefined at runtime instead of a rolldown MISSING_EXPORT
// at link time (`import { createHmac } from "crypto"` in code shared with the
// server was failing whole client builds; rolldown-vite moved its own build
// stubs to CJS for exactly this). Production is Vite's bare
// `module.exports = {}`. Dev uses the warning Proxy from Vite's dep OPTIMIZER
// stub rather than the throwing vite:resolve stub, on purpose: oj bundles the
// dev client graph, and the throwing variant turns a mere feature probe
// (`typeof createHmac === "function"`) into a page-breaking module-eval
// error. The Proxy sits on the prototype (Object.create) because CJS-to-ESM
// interop copies own properties and would flatten a bare Proxy to `{}`.
const ALS =
  "class AsyncLocalStorage{getStore(){return this._s}" +
  "run(s,cb,...a){const p=this._s;this._s=s;try{return cb(...a)}finally{this._s=p}}" +
  "enterWith(s){this._s=s}exit(cb,...a){const p=this._s;this._s=undefined;try{return cb(...a)}finally{this._s=p}}" +
  "disable(){this._s=undefined}}module.exports={AsyncLocalStorage};";
const SHIM_STREAM_WEB =
  "module.exports={ReadableStream:globalThis.ReadableStream,WritableStream:globalThis.WritableStream," +
  "TransformStream:globalThis.TransformStream,ByteLengthQueuingStrategy:globalThis.ByteLengthQueuingStrategy," +
  "CountQueuingStrategy:globalThis.CountQueuingStrategy};";
const SHIM_STREAM =
  "class S{on(){return this}once(){return this}emit(){return false}pipe(t){return t}end(){}write(){return true}" +
  "removeListener(){return this}destroy(){}}class Readable extends S{static from(){return new Readable()}}" +
  "class Writable extends S{}class Duplex extends S{}class Transform extends S{}" +
  "class PassThrough extends S{}class Stream extends S{}" +
  "module.exports={Readable,Writable,Duplex,Transform,PassThrough,Stream};";
const SHIM_PUNYCODE =
  "const id=(s)=>s;" +
  'module.exports={toUnicode:id,toASCII:id,encode:id,decode:id,ucs2:{decode:()=>[],encode:()=>""}};';
// Vite's builtin set is node's `builtinModules`, which carries the bare
// subpaths (`fs/promises`, `timers/promises`, `path/posix`, ...): a shared
// module spelling one of those without the `node:` prefix is the same failure
// family as the crypto shape. The union with the legacy hand list keeps
// deprecated aliases a runtime's `builtinModules` may not report.
const LEGACY_BUILTINS = (
  "assert buffer child_process cluster console constants crypto dgram dns domain events fs http http2 " +
  "https module net os path perf_hooks process punycode querystring readline repl stream stream/web " +
  "string_decoder sys timers tls tty url util v8 vm worker_threads zlib async_hooks"
).split(" ");
const BARE_BUILTIN_NAMES = new Set([...builtinModules.filter((n) => !n.includes(":")), ...LEGACY_BUILTINS]);
export const BARE_BUILTINS = new RegExp(`^(${[...BARE_BUILTIN_NAMES].join("|")})$`);
export function shimSource(spec, production) {
  const name = spec.replace(/^node:/, "");
  if (name === "async_hooks") return ALS;
  if (name === "stream/web") return SHIM_STREAM_WEB;
  if (name === "stream") return SHIM_STREAM;
  if (name === "punycode") return SHIM_PUNYCODE;
  if (production) return "module.exports = {};";
  // The specifier reaches this generated source only as a JSON string
  // literal: a hostile or malformed `node:` tail must not be able to break
  // out of (or into) the shim code.
  return (
    `const name = ${JSON.stringify(name)};\n` +
    "module.exports = Object.create(new Proxy({}, {\n" +
    "  get(_, key) {\n" +
    "    if (key !== '__esModule' && key !== '__proto__' && key !== 'constructor' && key !== 'splice') {\n" +
    '      console.warn(`Module "${name}" has been externalized for browser compatibility. ' +
    'Cannot access "${name}.${String(key)}" in client code.`);\n' +
    "    }\n" +
    "  }\n" +
    "}));"
  );
}
export function nodeBuiltinShims({ production = false } = {}) {
  return {
    name: "node-builtin-shims",
    resolveId: {
      filter: { id: { include: [/^node:/, BARE_BUILTINS] } },
      async handler(source, importer) {
        if (!/^node:/.test(source)) {
          if (!BARE_BUILTINS.test(source)) return null;
          // Vite stubs a builtin only after node resolution fails, so an
          // installed package sharing a builtin's name (npm `events`,
          // `punycode`) wins over the shim. `node:` ids skip the probe:
          // npm cannot own that scheme.
          const found = await this.resolve(source, importer, { skipSelf: true });
          if (found) return found;
        }
        // Normalize the spelling so `crypto` and `node:crypto` are ONE
        // module: the async_hooks and stream shims are stateful, and split
        // identities would give two ALS stores or instanceof mismatches.
        return V("node-shim", source.replace(/^node:/, ""));
      },
    },
    load: {
      filter: { id: /^\0oj-node-shim:/ },
      handler(id) {
        const v = parseV(id);
        return v && v.tag === "node-shim" ? { code: shimSource(v.path, production), moduleType: "js" } : null;
      },
    },
  };
}

export function pnpmStorePaths(workspaceRoot) {
  const paths = [];
  const pnpmDir = join(workspaceRoot, "node_modules/.pnpm");
  try {
    for (const e of readdirSync(pnpmDir)) {
      const nm = join(pnpmDir, e, "node_modules");
      if (existsSync(nm)) paths.push(nm);
    }
  } catch {}
  return paths;
}

export function workspaceRoot(app) {
  let best = app;
  for (let cur = app; ;) {
    const parent = dirname(cur);
    if (parent === cur) break;
    if (existsSync(join(parent, "node_modules"))) best = parent;
    cur = parent;
  }
  return best;
}

// Vite's build minifyCSS (lightningcss), on oj's CSS pipeline: a finished
// sheet minified for build.cssTarget.
const minifyCss = async (file, css, targets) =>
  (await import(STYLE_SCHEME + "///?" + new URLSearchParams({ file, minify: css, targets: JSON.stringify(targets) })))
    .css;

// `cssMinify`: Vite's build.cssMinify per environment ({ client, ssr }) and
// build.cssTarget ({ targets }); unset minifies nothing.
export function contentHashEmitter(clientDir, compileCss, base = "/", cssMinify = null) {
  const assetsDir = join(clientDir, "assets");
  const seen = new Set();
  const emitting = new Set();
  const cssUrls = [];

  const write = (absPath, buf) => {
    const ext = extname(absPath);
    const hash = createHash("sha256").update(buf).digest("hex").slice(0, 8);
    const name = basename(absPath, ext).replace(/[^\w.-]+/g, "_") + "-" + hash + ext;
    if (!seen.has(name)) {
      mkdirSync(assetsDir, { recursive: true });
      writeFileSync(join(assetsDir, name), buf);
      seen.add(name);
    }
    return base + "assets/" + name;
  };

  async function emit(absPath) {
    if (extname(absPath).toLowerCase() === ".css" && !emitting.has(absPath)) {
      return emitCss(absPath, readFileSync(absPath, "utf8"));
    }
    return write(absPath, readFileSync(absPath));
  }

  // A stylesheet's text as the file at absPath (a CSS module's scoped sheet
  // is not the file's bytes): compiled, its urls emitted, then hashed.
  async function emitCss(absPath, css) {
    emitting.add(absPath);
    try {
      if (compileCss && needsCssCompile(css)) css = await compileCss(absPath, css);
      // A compiled .scss/.less sheet ships as .css.
      const sheet = absPath.replace(/\.[^./\\]+$/, ".css");
      css = await rewriteCss(css, dirname(absPath));
      // Vite's finalizeCss: an emitted sheet is minified under the client's
      // cssMinify, ending in a newline (minifyCSS, not inlined).
      if (cssMinify?.client) css = (await minifyCss(absPath, css, cssMinify.targets)) + "\n";
      const url = write(sheet, Buffer.from(css, "utf8"));
      if (!cssUrls.includes(url)) cssUrls.push(url);
      return url;
    } finally {
      emitting.delete(absPath);
    }
  }
  emit.css = emitCss;
  // A `?inline` sheet's text: compiled and its urls emitted, as emitCss does,
  // but returned instead of written, and minified under the importing
  // environment's cssMinify with no trailing newline (css.ts minifyCSS).
  emit.inlineCss = async (absPath, css, server = false) => {
    if (compileCss && needsCssCompile(css)) css = await compileCss(absPath, css);
    css = await rewriteCss(css, dirname(absPath));
    if (cssMinify?.[server ? "ssr" : "client"]) css = await minifyCss(absPath, css, cssMinify.targets);
    return css;
  };

  async function rewriteCss(css, dir) {
    const re = /url\(\s*(['"]?)([^'")]+)\1\s*\)/g;
    let out = "",
      last = 0,
      m;
    while ((m = re.exec(css))) {
      out += css.slice(last, m.index);
      last = m.index + m[0].length;
      const t = m[2].trim();
      if (/^(data:|https?:|\/\/|#|\/)/.test(t)) {
        out += m[0];
        continue;
      }
      const clean = t.replace(/[?#].*$/, "");
      const suffix = t.slice(clean.length);
      const abs = resolve(dir, clean);
      if (!existsSync(abs)) {
        out += m[0];
        continue;
      }
      out += `url(${JSON.stringify((await emit(abs)) + suffix)})`;
    }
    return out + css.slice(last);
  }

  // A worker entry is bundled on its own (Vite's bundleWorkerEntry) by the
  // builder the caller installs, once per file, so the client and server
  // builds share one emitted URL.
  // `workerCode` is the bundle (inlined by `?worker&inline`), `worker` the
  // emitted file's URL.
  const workerCodes = new Map();
  const workerUrls = new Map();
  let bundleWorker = null;
  emit.setWorkerBundler = (fn) => {
    bundleWorker = fn;
  };
  emit.workerCode = (absPath) => {
    if (!workerCodes.has(absPath)) {
      workerCodes.set(
        absPath,
        (async () => {
          if (!bundleWorker) throw new Error(`oj: no worker bundler for ${absPath}`);
          return bundleWorker(absPath);
        })(),
      );
    }
    return workerCodes.get(absPath);
  };
  emit.worker = (absPath) => {
    if (!workerUrls.has(absPath)) {
      workerUrls.set(
        absPath,
        emit
          .workerCode(absPath)
          .then((code) => write(absPath.replace(/\.[^./\\]+$/, "") + ".js", Buffer.from(code, "utf8"))),
      );
    }
    return workerUrls.get(absPath);
  };

  emit.cssUrls = () => cssUrls.slice();
  return emit;
}

export function needsCssCompile(src) {
  return src.includes("tailwindcss") || src.includes("@tailwind") || src.includes("@plugin") || src.includes("@apply");
}

// The app's chunk policy for the client bundle. Vite applies no chunk
// grouping of its own: `build.rolldownOptions` passes through to rolldown in
// build and bundled dev alike (resolveRolldownOptions), so the grouping an
// app configures (vendor chunks, i18n buckets) is the app's to make and
// oj's to honor. Carried: `output.codeSplitting` (rolldown's current name;
// `advancedChunks` was its earlier spelling) and
// `experimental.chunkOptimization`. The client environment's options win
// over the top-level ones, Vite's environment resolution. The config is
// live in this process, so function-valued group names work as under Vite.
// `preserveEntrySignatures` defaults to false as Vite's client app build
// sets it (resolveRolldownOptions, before the user's options spread over
// it); left unset, rolldown warns INVALID_OPTION for any group with
// `includeDependenciesRecursively: false`. oj's client entry exports
// nothing, so false drops nothing.
export function clientChunkPolicy(config) {
  const top = config?.build?.rolldownOptions ?? {};
  const env = config?.environments?.client?.build?.rolldownOptions ?? {};
  const out = { ...top.output, ...env.output };
  const codeSplitting = out.codeSplitting ?? out.advancedChunks;
  const chunkOptimization = { ...top.experimental, ...env.experimental }.chunkOptimization;
  const preserveEntrySignatures = env.preserveEntrySignatures ?? top.preserveEntrySignatures ?? false;
  return {
    output: codeSplitting === undefined ? {} : { codeSplitting },
    input: {
      preserveEntrySignatures,
      ...(chunkOptimization === undefined ? {} : { experimental: { chunkOptimization } }),
    },
  };
}

// The dev bundle evaluates chunk-policy functions in a mode they were not
// necessarily written for: `apply: "build"` plugins never ran under serve,
// and a group computed from their output throws when that data is missing
// (a route-shared group populated by a build-only plugin is the shape).
// Vite's dev server never evaluates these functions at all, so a throwing
// group must not take the dev server down: it steps aside with one warning
// and the other groups still apply. The production build stays loud, as
// `vite build` is.
export function devSafeChunkPolicy(policy) {
  const cs = policy.output.codeSplitting;
  if (!cs || !Array.isArray(cs.groups)) return policy;
  const warned = new Set();
  const guard = (group, index, field, miss) => {
    const fn = group[field];
    return (...args) => {
      try {
        return fn(...args);
      } catch (e) {
        const name = typeof group.name === "string" ? group.name : `#${index}`;
        if (!warned.has(name)) {
          warned.add(name);
          process.stderr.write(
            `oj: codeSplitting group ${name} threw under dev and steps aside ` +
              `(a build-only plugin it expects did not run): ${e?.message ?? e}\n`,
          );
        }
        return miss;
      }
    };
  };
  const groups = cs.groups.map((g, i) => {
    if (!g || typeof g !== "object") return g;
    const safe = { ...g };
    if (typeof g.name === "function") safe.name = guard(g, i, "name", undefined);
    if (typeof g.test === "function") safe.test = guard(g, i, "test", false);
    return safe;
  });
  return { ...policy, output: { ...policy.output, codeSplitting: { ...cs, groups } } };
}
