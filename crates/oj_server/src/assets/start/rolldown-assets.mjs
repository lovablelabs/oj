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
      ".png": "image/png", ".jpg": "image/jpeg", ".jpeg": "image/jpeg", ".gif": "image/gif",
      ".webp": "image/webp", ".avif": "image/avif", ".svg": "image/svg+xml", ".ico": "image/x-icon",
      ".woff": "font/woff", ".woff2": "font/woff2", ".ttf": "font/ttf", ".otf": "font/otf",
    }[extname(abs).toLowerCase()] || "application/octet-stream";
  return `data:${mime};base64,${buf.toString("base64")}`;
};

const makeUrlFor = ({ mode, fsBase, emit }) => async (abs) => (mode === "dev" ? fsBase + abs : emit(abs));

// A worker's URL, Vite's worker plugin load. Unbundled dev: the file's URL on
// the dev pipeline (fileToUrl: root-relative, `/@fs` outside the root), which
// serves the compiled module worker and is what the SSR host renders. A build:
// the worker entry bundled on its own (bundleWorkerEntry) and emitted once.
const makeWorkerUrlFor = ({ mode, root, emit }) => async (abs) => {
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

export function assetsPlugin({ mode = "dev", server = false, fsBase = "/@oj-start/fs", emit, cssUrls, root } = {}) {
  const urlFor = makeUrlFor({ mode, fsBase, emit });
  const workerUrlFor = makeWorkerUrlFor({ mode, root, emit });
  return {
    name: "oj-assets",
    resolveId: {
      filter: { id: { include: [WORKER_RE, URL_RE, RAW_RE, INLINE_RE, NO_INLINE_RE, ASSET_EXT, /\.css(\?|$)/] } },
      async handler(source, importer, options) {
        if (options?.custom?.ojAsset) return null;
        let tag = null;
        const worker = WORKER_RE.exec(source);
        // Vite's worker load: a bundled environment checks `&inline` before
        // `&url`; unbundled dev ignores `&inline`.
        if (worker) {
          tag = worker[1] + (mode !== "dev" && INLINE_RE.test(source) ? "-inline" : URL_RE.test(source) ? "-url" : "");
        }
        else if (RAW_RE.test(source)) tag = "raw";
        else if (URL_RE.test(source) || NO_INLINE_RE.test(source)) tag = "url";
        else if (INLINE_RE.test(source)) tag = "inline";
        else if (ASSET_EXT.test(source)) tag = "url";
        else if (/\.css(\?|$)/.test(source)) tag = "css";
        if (!tag) return null;
        const clean = source.replace(/\?.*$/, "");
        const r = await this.resolve(clean, importer, { skipSelf: true, custom: { ojAsset: true } });
        if (!r) return null;
        return V(tag, r.id);
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
        if (v.tag === "inline") return js(`export default ${JSON.stringify(dataUri(v.path))};`);
        if (v.tag === "worker-url" || v.tag === "sharedworker-url") {
          return js(`export default ${JSON.stringify(await workerUrlFor(v.path))};`);
        }
        if (v.tag === "worker-inline" || v.tag === "sharedworker-inline") {
          return js(inlineWorkerModule(v.tag === "sharedworker-inline" ? "SharedWorker" : "Worker", await emit.workerCode(v.path)));
        }
        if (v.tag === "worker" || v.tag === "sharedworker") {
          const ctor = v.tag === "sharedworker" ? "SharedWorker" : "Worker";
          const url = JSON.stringify(await workerUrlFor(v.path));
          return js(
            `export default function WorkerWrapper(options) { return new ${ctor}(${url}, { type: "module", name: options?.name }); }`,
          );
        }
        if (v.tag === "css") {
          if (!server) {
            const href = await urlFor(v.path);
            if (cssUrls && !cssUrls.includes(href)) cssUrls.push(href);
          }
          return js("export default {};");
        }
        return null;
      },
    },
  };
}

export function makeVitePlugins({ container, fallback, appRoot, mode = "dev", fsBase = "/@oj-start/fs", emit } = {}) {
  const urlFor = makeUrlFor({ mode, fsBase, emit });
  const warnedVirtual = new Set();
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
      // Run user plugins' buildStart before any module loads, so compile-on-
      // startup plugins (e.g. i18n) have populated the state their load() serves.
      if (container?.buildStart) await container.buildStart();
      if (fallback?.buildStart && fallback !== container) await fallback.buildStart();
    },
    async buildEnd(error) {
      if (container?.buildEnd) await container.buildEnd(error);
    },
    async renderStart(outputOptions, inputOptions) {
      if (container?.renderStart) await container.renderStart(outputOptions, inputOptions);
    },
    resolveId: {
      filter: { id: { include: [/\.svg\?react$/, /^virtual:/, /^\0/] } },
      async handler(source, importer, options) {
        if (options?.custom?.ojSvg) return null;
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
          return { code, moduleType: "jsx" };
        }
        if (/\.svg$/.test(id) && !id.startsWith("\0")) return svgModule(id, id);
        // A user plugin's load() may override a real on-disk source file (Vite:
        // load runs before the fs read). Consult it for user files in the build
        // too, so compile-on-startup plugins produce the same output as dev.
        if (container && !id.startsWith("\0") && !id.includes("/node_modules/")) {
          const cleanId = id.replace(/\?.*$/, "");
          if (/\.(jsx?|mjs|tsx?)$/.test(cleanId)) {
            let code = await container.load(cleanId);
            if (code == null && fallback) code = await fallback.load(cleanId);
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
  "module.exports={toUnicode:id,toASCII:id,encode:id,decode:id,ucs2:{decode:()=>[],encode:()=>\"\"}};";
// Vite's builtin set is node's `builtinModules`, which carries the bare
// subpaths (`fs/promises`, `timers/promises`, `path/posix`, ...): a shared
// module spelling one of those without the `node:` prefix is the same failure
// family as the crypto shape. The union with the legacy hand list keeps
// deprecated aliases a runtime's `builtinModules` may not report.
const LEGACY_BUILTINS =
  ("assert buffer child_process cluster console constants crypto dgram dns domain events fs http http2 " +
    "https module net os path perf_hooks process punycode querystring readline repl stream stream/web " +
    "string_decoder sys timers tls tty url util v8 vm worker_threads zlib async_hooks").split(" ");
const BARE_BUILTIN_NAMES = new Set([
  ...builtinModules.filter((n) => !n.includes(":")),
  ...LEGACY_BUILTINS,
]);
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
        return v && v.tag === "node-shim"
          ? { code: shimSource(v.path, production), moduleType: "js" }
          : null;
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
  for (let cur = app; ; ) {
    const parent = dirname(cur);
    if (parent === cur) break;
    if (existsSync(join(parent, "node_modules"))) best = parent;
    cur = parent;
  }
  return best;
}

export function contentHashEmitter(clientDir, compileCss, base = "/") {
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
      emitting.add(absPath);
      try {
        let css = readFileSync(absPath, "utf8");
        if (compileCss && needsCssCompile(css)) css = await compileCss(absPath, css);
        const url = write(absPath, Buffer.from(await rewriteCss(css, dirname(absPath)), "utf8"));
        if (!cssUrls.includes(url)) cssUrls.push(url);
        return url;
      } finally {
        emitting.delete(absPath);
      }
    }
    return write(absPath, readFileSync(absPath));
  }

  async function rewriteCss(css, dir) {
    const re = /url\(\s*(['"]?)([^'")]+)\1\s*\)/g;
    let out = "", last = 0, m;
    while ((m = re.exec(css))) {
      out += css.slice(last, m.index);
      last = m.index + m[0].length;
      const t = m[2].trim();
      if (/^(data:|https?:|\/\/|#|\/)/.test(t)) { out += m[0]; continue; }
      const clean = t.replace(/[?#].*$/, "");
      const suffix = t.slice(clean.length);
      const abs = resolve(dir, clean);
      if (!existsSync(abs)) { out += m[0]; continue; }
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
