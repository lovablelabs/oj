// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import http from "node:http";
import https from "node:https";
import { createReadStream, existsSync, fstatSync, openSync, readFileSync, statSync, unlinkSync, write as fsWrite, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { readFile, stat as fsStat } from "node:fs/promises";
import { createRequire, isBuiltin } from "node:module";
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, isAbsolute, join, resolve as pathResolve } from "node:path";
import readline from "node:readline";
import { AsyncLocalStorage } from "node:async_hooks";
import { stripVTControlCharacters } from "node:util";
import { EventEmitter } from "node:events";

// Parent watchdog (esbuild's --ppid model), armed before anything can await:
// the stdin-EOF net at the bottom of this file only arms after every top-level
// await, and a boot stuck awaiting an RPC its dead parent will never answer
// writes nothing (so no EPIPE crash) and reads nothing (so no EOF) — that host
// survived as an orphan forever. When the spawning oj process dies, the host is
// reparented (ppid flips to the reaper); poll for that and hard-exit. unref'd
// so the timer itself never keeps the event loop alive.
const spawnPpid = process.ppid;
setInterval(() => {
  if (process.ppid !== spawnPpid) process.kill(process.pid, "SIGKILL");
}, 1000).unref();

// The slice of Vite's mergeConfigRecursively these assets need (twin copies:
// one in vite-extract.mjs, one in plugin-host.mjs — keep them byte-identical):
// null and undefined override values are skipped (a `key: null` override must
// not clobber a set value), arrays concatenate, PLAIN objects (Vite's
// isObject: `Object.prototype.toString === "[object Object]"`) merge
// recursively — a class instance or RegExp is a value, so a later one
// replaces the earlier instead of being spread into a bare `{}` — scalars and
// functions take the later value, and a `true` on either side of ssr/resolve
// `noExternal`/`external` wins over lists (mergeConfigRecursively's special
// case).
const environmentPathRE = /^environments\.[^.]+$/;
const isPlainObject = (value) => Object.prototype.toString.call(value) === "[object Object]";
function mergeConfigLite(defaults, overrides, rootPath = "") {
  const merged = { ...defaults };
  for (const key of Object.keys(overrides ?? {})) {
    const value = overrides[key];
    if (value == null) continue;
    const existing = merged[key];
    if (
      (key === "noExternal" || key === "external") &&
      (rootPath === "ssr" || rootPath === "resolve") &&
      (existing === true || value === true)
    ) {
      merged[key] = true;
      continue;
    }
    if (existing == null) merged[key] = value;
    else if (Array.isArray(existing) || Array.isArray(value)) {
      merged[key] = [
        ...(Array.isArray(existing) ? existing : [existing]),
        ...(Array.isArray(value) ? value : [value]),
      ];
    } else if (isPlainObject(existing) && isPlainObject(value)) {
      // As in Vite: an `environments.<name>` node restarts path tracking, so
      // `environments.ssr.resolve.noExternal` merges like `resolve.noExternal`.
      merged[key] = mergeConfigLite(
        existing,
        value,
        rootPath && !environmentPathRE.test(rootPath) ? `${rootPath}.${key}` : key,
      );
    } else merged[key] = value;
  }
  return merged;
}

const pluginsPath = process.argv[2];
const initial = JSON.parse(process.argv[3] ?? "{}");

process.env.VITE_CONFIG_NATIVE_IGNORE_WARNING ??= "true";
// Vite's ConfigEnv (config.ts): `{ mode, command, isSsrBuild, isPreview }`.
// oj builds the ssr environment in its own host, so that host's build is the
// ssr build (Vite: `command === "build" && !!config.build.ssr`).
const hostEnvName = initial.environment?.name ?? "client";
const env = { command: "serve", mode: "development", ...(initial.env ?? {}) };
env.isSsrBuild = env.isSsrBuild ?? (env.command === "build" && hostEnvName !== "client");
env.isPreview = env.isPreview ?? false;

// The twin NODE_ENV block below reads the command through this alias (the
// host gets it from the ConfigEnv; the extractor from argv).
const command = env.command;
// Vite's defaultNodeEnv follows the COMMAND, never the mode (node.js
// resolveConfig: serve resolves with defaultNodeEnv "development", build with
// "production"), so `build --mode staging` still sees NODE_ENV=production and
// isProduction=true. Pre-set before any plugin or config code evaluates, as
// resolveConfig sets it before loadConfigFromFile (`if (!isNodeEnvSet)
// process.env.NODE_ENV = defaultNodeEnv`). The untouched pre-set is UNSET
// again right before a real vite.resolveConfig run (unsetOjNodeEnvForResolve):
// it must not make Vite compute isNodeEnvSet=true — its VITE_USER_NODE_ENV
// handling stays live — while a config module that ASSIGNED
// process.env.NODE_ENV at module scope keeps its value through resolveConfig
// (Vite snapshots isNodeEnvSet before the load); resolveConfig then re-sets
// the identical default itself from the defaultNodeEnv oj passes. (Twin
// copies: one in vite-extract.mjs, one in plugin-host.mjs — keep them
// byte-identical.)
const defaultNodeEnv = command === "build" ? "production" : "development";
const nodeEnvWasSet = !!process.env.NODE_ENV;
if (!nodeEnvWasSet) process.env.NODE_ENV = defaultNodeEnv;
const unsetOjNodeEnvForResolve = () => {
  if (!nodeEnvWasSet && process.env.NODE_ENV === defaultNodeEnv) delete process.env.NODE_ENV;
};
// Snapshot after the host's own env tweaks (the NODE_ENV pre-set included) so
// the config()-hook delta reports only plugin mutations, not bootstrap noise.
const ssrEnvBase = { ...process.env };

// resolve.alias from the app's own vite config (loaded below for its plugins).
// oj applies aliases in its Rust resolver and does not forward them in the
// config it hands the host, so createResolver — which plugins like wyw-in-js use
// to resolve modules during CSS evaluation — would otherwise see no aliases.
let userResolveAlias = null;
// The user's config file as loaded (before oj's overlay and any resolve):
// buildEnvironments consults it to tell a user-chosen option from a default.
let userViteConfig = null;

// This process hosts untrusted third-party plugin code. A plugin that spawns a
// worker or a floating promise which throws asynchronously would otherwise take
// the whole host down, and with it every other plugin. The host's own request
// handling is synchronous and locally guarded, so a plugin's stray async
// failure is logged and swallowed rather than made fatal.
process.on("uncaughtException", (e) => {
  process.stderr.write(`oj plugin host: uncaught plugin error (ignored): ${(e && (e.stack || e.message)) || e}\n`);
});
process.on("unhandledRejection", (e) => {
  process.stderr.write(`oj plugin host: unhandled plugin rejection (ignored): ${(e && (e.stack || e.message)) || e}\n`);
});

// Start mode hosts only configureServer middleware for a framework config oj
// owns (TanStack Start): the framework plugins drive the module graph and SSR
// themselves, so their config lifecycle hooks are tolerated per-plugin instead
// of aborting the load, keeping the editor middleware (dev-server bridge) alive.
const ojStartMode = initial.ojStartMode === true;

const _ojTTY = process.stderr.isTTY && !process.env.NO_COLOR;
const OJ = _ojTTY ? "\x1b[48;2;255;255;255m\x1b[1;38;2;42;51;212m oj \x1b[0m" : "oj";

// oj does not implement Vite's `experimental.bundledDev` (Rolldown bundled
// client dev mode): oj bundles the client itself, so bundledDev is redundant
// under oj, and left on the app's TanStack manifest emits a `/assets/index.js`
// client script oj never builds nor serves. Coerce it off wherever oj drives the
// config so the standard Vite dev client entry (which oj serves) is used, and
// warn once per session. Returns whether it flipped a truthy value.
let _bundledDevWarned = false;
function coerceBundledDevOff(cfg) {
  if (!cfg || typeof cfg !== "object") return false;
  // bundledDev is Vite's serve-only bundled-client mode; only serve derives
  // `environments.*.isBundled` from it. During a build Vite sets `isBundled`
  // true on every environment regardless (the normal production state), so
  // clearing it there is wrong and would emit a dev-worded warning on a build.
  if (command !== "serve") return false;
  let flipped = false;
  if (cfg.experimental && cfg.experimental.bundledDev) {
    cfg.experimental.bundledDev = false;
    flipped = true;
  }
  // Vite derives `environments.client.isBundled` from `experimental.bundledDev`
  // in `resolveConfig`; a resolved config carries it and the DevEnvironment
  // constructor (plus TanStack's manifest plugin) reads it directly.
  for (const name of Object.keys(cfg.environments || {})) {
    if (cfg.environments[name] && cfg.environments[name].isBundled) {
      cfg.environments[name].isBundled = false;
      flipped = true;
    }
  }
  if (flipped && !_bundledDevWarned) {
    _bundledDevWarned = true;
    process.stderr.write(`${OJ}: experimental.bundledDev is not supported; using the standard dev client entry instead\n`);
  }
  return flipped;
}

// The one writer for every oj protocol line (RPC replies, ctx-RPC requests,
// and the ojServeInfo/ojServer/ojWs pushes). Plugin code shares this stdout,
// so the frame defends the control plane on both sides: the leading newline
// terminates any unterminated partial line a plugin left on the stream (a
// spliced frame would otherwise be silently dropped), and the per-session
// token (OJ_CONTROL_TOKEN, minted by the Rust spawn) marks the line as oj's —
// the Rust reader ignores unframed lines, so a plugin's print, or
// attacker-controlled content a plugin echoes, can never forge a reply or a
// push. Forging with the token requires reading this process's env: the same
// trust domain as running plugin code at all. Without the env (tests driving
// the host directly) the frame degrades to the bare line protocol.
const CONTROL_TOKEN = process.env.OJ_CONTROL_TOKEN || "";
function ctl(obj) {
  process.stdout.write("\n" + CONTROL_TOKEN + JSON.stringify(obj) + "\n");
}

// Top-level init MILESTONES ({ ojInitProgress }), pushed at each real step of
// the boot (script start, plugins loaded, every config-phase hook). Rust's
// stall monitor measures its wedge window from the last one: a healthy slow
// boot that keeps hitting milestones is never declared wedged however long
// it takes, while a host gone silent for a full RPC-scale window pre-init
// flips the wedge evidence (releasing e.g. the Start prewarm hold) — the
// last stage's name doubles as the diagnosis of WHERE the boot hangs. Not
// timer-driven on purpose: a heartbeat would keep ticking through a hang
// that leaves the event loop alive and defeat the evidence.
let ojInitDone = false;
const initStage = (stage) => {
  if (!ojInitDone) ctl({ ojInitProgress: stage });
};
initStage("start");

let ssrBridgeDir = null;
let ssrContainer = null;
let ssrEnvDelta = {};
let ssrResolveReady;
const ssrReady = new Promise((r) => { ssrResolveReady = r; });
if (ojStartMode && initial.ssrBridge && initial.ssrBridge.dir) {
  const dir = initial.ssrBridge.dir;
  try {
    const repFd = openSync(join(dir, "rep.fifo"), "r+");
    const reqFd = openSync(join(dir, "req.fifo"), "r+");
    ssrBridgeDir = dir;
    const writeAll = (buf) => new Promise((resolve) => {
      let off = 0;
      const step = () => fsWrite(repFd, buf, off, buf.length - off, null, (e, n) => {
        if (e) {
          process.stderr.write(`${OJ} ssr bridge write failed: ${e}\n`);
          return resolve();
        }
        off += n;
        off < buf.length ? step() : resolve();
      });
      step();
    });
    let writeChain = Promise.resolve();
    const reply = (obj) => {
      const json = Buffer.from(JSON.stringify(obj));
      const frame = Buffer.allocUnsafe(4 + json.length);
      frame.writeUInt32LE(json.length, 0);
      json.copy(frame, 4);
      writeChain = writeChain.then(() => writeAll(frame));
    };
    const SSR_METHODS = new Set(["resolveId", "load", "transform", "transformUserCode"]);
    const dispatch = async ({ id, method, args }) => {
      await ssrReady;
      try {
        if (method === "__env") return reply({ id, value: ssrEnvDelta });
        if (method === "__define") {
          return reply({ id, value: {
            ...resolvedConfig.define,
            ...resolvedConfig.environments?.ssr?.define,
          } });
        }
        if (method === "__heap") {
          return reply({ id, value: (await import("node:v8")).default.getHeapStatistics() });
        }
        if (!ssrContainer || !SSR_METHODS.has(method)) return reply({ id, value: null });
        reply({ id, value: (await ssrContainer[method](...(args ?? []))) ?? null });
      } catch (e) {
        reply({ id, error: String((e && e.stack) || e) });
      }
    };
    let acc = Buffer.alloc(0);
    createReadStream(null, { fd: reqFd, autoClose: false })
      .on("data", (chunk) => {
        acc = acc.length ? Buffer.concat([acc, chunk]) : chunk;
        while (acc.length >= 4) {
          const len = acc.readUInt32LE(0);
          if (acc.length < 4 + len) break;
          let msg = null;
          try { msg = JSON.parse(acc.subarray(4, 4 + len).toString("utf8")); } catch {}
          acc = acc.subarray(4 + len);
          if (msg && msg.id != null) dispatch(msg);
        }
      })
      .on("error", (e) => process.stderr.write(`${OJ} ssr bridge read failed: ${e}\n`));
  } catch (e) {
    process.stderr.write(`${OJ} ssr bridge unavailable: ${(e && e.message) || e}\n`);
    try { writeFileSync(join(dir, "disabled"), "1"); } catch {}
  }
}

// Vite's ResolvedConfig exposes createResolver(): a standalone module resolver
// built from the config's resolve options. Plugins that must resolve modules
// outside a hook context use it (e.g. wyw-in-js/linaria resolves the imports it
// reaches while evaluating `styled`/`css` template literals). It returns
// `(id, importer, aliasOnly, ssr) => Promise<string|undefined>` where the string
// is an absolute path. A faithful-enough subset — alias map + extension probe,
// with a Node fallback for bare specifiers — covers what those plugins need.
const RESOLVE_EXTS = [".mjs", ".js", ".mts", ".ts", ".jsx", ".tsx", ".cjs", ".cts", ".json"];

function aliasEntries(alias) {
  if (!alias) return [];
  if (Array.isArray(alias)) return alias.map((e) => [e.find, e.replacement]);
  return Object.entries(alias);
}

function applyAlias(id, entries) {
  for (const [find, replacement] of entries) {
    if (typeof replacement !== "string") continue;
    if (find instanceof RegExp) {
      if (find.test(id)) return id.replace(find, replacement);
      continue;
    }
    if (typeof find !== "string") continue;
    // oj's config extractor emits directory aliases with a trailing slash
    // (`"@/"` -> "<abs>/src/modules/"); normalize so both `"@"` and `"@/"`
    // forms match `@/foo` and join cleanly.
    const f = find.endsWith("/") ? find.slice(0, -1) : find;
    if (id === f) return replacement;
    if (id.startsWith(f + "/")) {
      const rest = id.slice(f.length + 1);
      const rep = replacement.endsWith("/") ? replacement : replacement + "/";
      return rep + rest;
    }
  }
  return id;
}

function probeFile(p, exts) {
  const st = statSync(p, { throwIfNoEntry: false });
  if (st?.isFile()) return p;
  if (st?.isDirectory()) {
    for (const ext of exts) {
      const idx = join(p, "index" + ext);
      if (existsSync(idx)) return idx;
    }
    return null;
  }
  for (const ext of exts) {
    if (existsSync(p + ext)) return p + ext;
  }
  return null;
}

function makeCreateResolver(config) {
  let entries = aliasEntries(config.resolve?.alias);
  if (entries.length === 0) entries = aliasEntries(userResolveAlias);
  const exts =
    Array.isArray(config.resolve?.extensions) && config.resolve.extensions.length
      ? config.resolve.extensions
      : RESOLVE_EXTS;
  const root = config.root ?? process.cwd();
  return function createResolver() {
    return async (id, importer, _aliasOnly, _ssr) => {
      if (!id) return undefined;
      let spec = id.split("?", 1)[0];
      if (spec.startsWith("\0") || spec.startsWith("/@")) return undefined;
      spec = applyAlias(spec, entries);
      const baseDir = importer
        ? dirname(importer.startsWith("file://") ? fileURLToPath(importer) : importer)
        : root;
      if (spec.startsWith(".")) return probeFile(pathResolve(baseDir, spec), exts) ?? undefined;
      if (isAbsolute(spec)) return probeFile(spec, exts) ?? undefined;
      try {
        return createRequire(join(baseDir, "__oj_resolver__.js")).resolve(spec);
      } catch {
        return undefined;
      }
    };
  };
}

function withResolvedDefaults(config) {
  const c = config ?? {};
  const merged = mergeConfigLite(
    {
      command: env.command,
      mode: env.mode,
      root: c.root ?? initial.config?.root ?? process.cwd(),
      base: "/",
      // Vite: `isProduction = process.env.NODE_ENV === "production"` (after
      // its command-based default fill), never derived from the mode — a
      // custom `--mode staging` build is still a production build.
      isProduction: process.env.NODE_ENV === "production",
      experimental: {},
      // Vite's resolved `build` carries defaults plugins read in configResolved
      // (e.g. UnoCSS resolves `config.build.outDir` and reads
      // `config.build.rollupOptions.output`). User config overrides these.
      build: {
        target: "modules",
        outDir: "dist",
        assetsDir: "assets",
        assetsInlineLimit: 4096,
        cssCodeSplit: true,
        sourcemap: false,
        rollupOptions: {},
        minify: "esbuild",
        reportCompressedSize: true,
        chunkSizeWarningLimit: 500,
      },
      server: { headers: {} },
      define: {},
      resolve: {},
      optimizeDeps: {},
      ssr: {},
      env: {},
      // Vite's resolved config always carries these; plugins read them in
      // configResolved/configureServer (e.g. `config.plugins.findIndex`,
      // `config.environments.client`). Absent, those hooks throw and get
      // skipped though they only meant to inspect the shape.
      plugins: [],
      environments: { client: {}, ssr: {} },
    },
    c,
  );
  if (typeof merged.createResolver !== "function") merged.createResolver = makeCreateResolver(merged);
  // Vite resolves `publicDir` to an absolute path (default `<root>/public`);
  // plugins like @crxjs read it directly to locate manifest assets (icons,
  // locales). `false` disables it, matching Vite.
  if (merged.publicDir !== false) {
    const pd = typeof merged.publicDir === "string" && merged.publicDir.length > 0 ? merged.publicDir : "public";
    merged.publicDir = isAbsolute(pd) ? pd : pathResolve(merged.root, pd);
  }
  // Vite's resolved config carries a `logger`; plugins (e.g. the cloudflare
  // plugin's ViteMiniflareLogger) call `config.logger.info/warn/error`.
  if (!merged.logger || typeof merged.logger.info !== "function") {
    const w = (...a) => process.stderr.write(a.map(String).join(" ") + "\n");
    merged.logger = {
      info: () => {}, warn: w, warnOnce: w, error: w,
      clearScreen: () => {}, hasErrorLogged: () => false, hasWarned: false,
    };
  }
  return merged;
}

const envName = initial.environment?.name ?? "client";
const environment = {
  name: envName,
  mode: initial.environment?.mode ?? env.mode,
  config: withResolvedDefaults(
    mergeConfigLite(initial.config ?? {}, (initial.config?.environments ?? {})[envName] ?? {}),
  ),
};
// Vite tags each environment's resolved config with a `consumer` ("client" or
// "server"); plugins like @vitejs/plugin-react read `env.config.consumer`
// directly in applyToEnvironment, so it must be present or they throw.
environment.config.consumer =
  environment.config.consumer ?? (envName === "client" ? "client" : "server");
// Vite's DevEnvironment carries `transformRequest`, which runs the module
// pipeline for one id. oj otherwise stubs it, but a plugin's transform can
// depend on it: TanStack Start's server-fn compiler, in dev, calls
// `this.environment.transformRequest(<id>?tss-server-fn-lookup)` so its
// `capture-server-fn-module-lookup` transform hook ingests a dependency (e.g.
// createMiddleware.js) into the compiler's module cache BEFORE it reads that
// dependency via getModuleInfo. Stubbed to a no-op, the ingest never happens,
// so correctness falls to module ordering -- and a cold concurrent first load
// that compiles createCsrfMiddleware.js before its createMiddleware.js
// dependency throws `could not load module info`, which oj serves as a hard 500
// that permanently breaks hydration (~1% of cold loads). Running the transform
// fires the capture hook and populates the cache on demand, as Vite does. Only
// the capture hook matches the `?tss-server-fn-lookup` id (the compiler's own
// transform excludes it), so there is no recompile or recursion here.
environment.transformRequest = async (rawId) => {
  const id = String(rawId ?? "");
  if (!id) return null;
  const qIdx = id.indexOf("?");
  const bareId = qIdx === -1 ? id : id.slice(0, qIdx);
  const query = qIdx === -1 ? "" : id.slice(qIdx);
  // Vite's transformRequest is resolve -> load -> transform (doTransform). Mirror
  // it: (1) the plugin `load` chain serves virtual/plugin-served ids (a `\0`
  // module) and (2) the file backs an absolute path, a `/@fs/` url, or a
  // root-relative url (Vite's asSrc). (3) A bare/relative specifier or a url that
  // neither serves nor exists is RESOLVED like Vite and re-entered. An id that
  // already loads or is an on-disk file is used as-is, so an already-resolved id
  // -- what TanStack passes, and the key its getModuleInfo looks up -- is never
  // rewritten.
  let source = null;
  const loaded = await loadFull(bareId);
  if (loaded && loaded.code != null) {
    source = loaded.code;
  } else {
    const root = resolvedConfig?.root ?? environment.config?.root ?? process.cwd();
    // `/foo` is both a valid absolute fs path and a Vite root-relative url, so
    // probe a real absolute file (TanStack passes bare `p.display()` paths)
    // BEFORE treating a leading `/` as root-relative (Vite's asSrc).
    let filePath = null;
    if (bareId.startsWith("/@fs/")) {
      const p = bareId.slice(4);
      if (existsSync(p)) filePath = p;
    } else if (isAbsolute(bareId) && existsSync(bareId)) {
      filePath = bareId;
    } else if (bareId.startsWith("/")) {
      const p = pathResolve(root, "." + bareId);
      if (existsSync(p)) filePath = p;
    }
    if (filePath) {
      try {
        source = readFileSync(filePath, "utf8");
      } catch {
        return null;
      }
    } else {
      // Not loadable and not on disk: resolve it like Vite, then re-enter with
      // the resolved id (`ctx.resolve` runs the plugin chain then oj's resolver).
      const r = await ctx.resolve(bareId, undefined);
      if (r && r.id && r.id !== bareId) return environment.transformRequest(r.id + query);
      return null;
    }
  }
  const out = JSON.parse(await transform(source, bareId + query, null));
  return { code: out.code };
};
// The resolved config (defaults + plugin `config` hooks). configureServer must
// receive this, not the raw initial.config, so plugins reading resolved-only
// fields (experimental, environments, plugins) don't throw. Seeded with the
// resolved environment config until runConfigHooks recomputes it.
let resolvedConfig = environment.config;

// When the app has Vite installed and its config is a vite.config, ask that Vite
// to resolve the user's config once (fresh plugin instances, whose config hooks
// run in that resolve) and use the result as the base the plugins see: real
// build.outDir/sourcemap/rollupOptions, cacheDir, env, envPrefix, assetsInclude(),
// css, ssr, worker, createResolver, logger. oj's own plugin instances are spliced
// in and their config hooks still run on top. Without Vite the synthesized
// defaults stand. OJ_PLUGIN_SYNTH_CONFIG=1 forces the synthesized path.
let userResolvedViteConfig = null;
async function loadUserResolvedViteConfig() {
  if (initial.pluginsFormat !== "vite" || process.env.OJ_PLUGIN_SYNTH_CONFIG === "1") return null;
  const appRoot = initial.config?.root ?? process.cwd();
  let vite;
  try {
    vite = await import(createRequire(appRoot + "/package.json").resolve("vite"));
  } catch {
    return null;
  }
  if (typeof vite.resolveConfig !== "function") return null;
  try {
    // Vite's own NODE_ENV handling runs inside resolveConfig; hand it the
    // command-based defaultNodeEnv (never the mode) and lift oj's untouched
    // pre-set first so it computes isNodeEnvSet like a real CLI run.
    unsetOjNodeEnvForResolve();
    const rc = await vite.resolveConfig(
      { root: appRoot, configFile: pluginsPath, mode: env.mode, logLevel: "silent" },
      env.command,
      env.mode,
      defaultNodeEnv,
    );
    return rc && typeof rc === "object" ? rc : null;
  } catch (e) {
    process.stderr.write(`${OJ} plugin host: vite.resolveConfig failed, using synthesized config: ${(e && e.message) || e}\n`);
    return null;
  } finally {
    // A throwing resolveConfig may leave the unset behind; the fallback paths
    // below still evaluate config/plugin code that must see a value.
    if (!process.env.NODE_ENV) process.env.NODE_ENV = defaultNodeEnv;
  }
}

// Vite's `externalize-deps` config-bundling plugin, mirrored (one copy lives
// in vite-extract.mjs, its twin in plugin-host.mjs; both assets are
// deliberately self-contained -- keep the two copies byte-identical).
// `packages: "external"` kept bare specifiers bare, so the bundle -- imported
// from the cache dir -- re-resolved them from there at import time. A config
// that relatively imports a sibling workspace package's source (a monorepo
// shape) inlines that source, whose bare deps live under the sibling's own
// node_modules, unreachable from the cache dir. Resolve every bare import from
// its importer at bundle time and externalize the resolved absolute path as a
// file:// URL instead (the output is always ESM, so an external require()
// would only reach esbuild's throwing __require shim). First-party
// resolutions -- TS/TSX sources, which Node cannot import, and anything
// outside node_modules, e.g. a tsconfig-paths alias -- are bundled like
// relative imports, as is .json (an externalized ESM .json import would need
// an import attribute).
function externalizeDepsPlugin() {
  const resolving = Symbol("oj-externalize-deps");
  const warned = new Set();
  return {
    name: "externalize-deps",
    setup(build) {
      build.onResolve({ filter: /^[^./#]/ }, async (args) => {
        // Re-entrant call from build.resolve below: let esbuild resolve it.
        if (args.pluginData === resolving) return null;
        const { path: id, importer, kind } = args;
        if (!importer || isAbsolute(id)) return null;
        if (id.startsWith("node:") || id.startsWith("bun:") || isBuiltin(id)) {
          return { path: id, external: true };
        }
        // npm: specifiers stay bare-external (Vite keeps them bare too); any
        // other scheme-shaped id (data:, ...) is esbuild's to handle natively
        // -- pathToFileURL on those would produce garbage.
        if (id.startsWith("npm:")) return { path: id, external: true };
        if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(id)) return null;
        let resolved = null;
        try {
          resolved = await build.resolve(id, {
            importer,
            resolveDir: dirname(importer),
            kind,
            pluginData: resolving,
          });
        } catch {}
        if (!resolved || resolved.errors.length > 0 || !resolved.path) {
          // Unresolvable here; keep the bare id external (the old behavior) --
          // it may still resolve at import time, and failing the bundle would
          // regress configs that load today. Say so once per id: if it does
          // fail at import time, that error names a deleted tmp bundle, not
          // the file that wrote the import.
          if (!warned.has(id)) {
            warned.add(id);
            const detail = resolved?.errors?.[0]?.text ? ` (${resolved.errors[0].text})` : "";
            process.stderr.write(
              `oj: vite.config: could not resolve "${id}" imported from ${importer}${detail}; kept as a bare import that must resolve when the bundled config loads\n`,
            );
          }
          return { path: id, external: true };
        }
        if (resolved.external) return { path: resolved.path, external: true };
        // Another resolver's non-file namespace is not ours to externalize.
        if (resolved.namespace && resolved.namespace !== "file") return null;
        if (
          /\.(?:ts|tsx|mts|cts)$/.test(resolved.path) ||
          !/[\\/]node_modules[\\/]/.test(resolved.path) ||
          resolved.path.endsWith(".json")
        ) {
          return { path: resolved.path };
        }
        return { path: pathToFileURL(resolved.path).href, external: true };
      });
    },
  };
}

// Vite's `inject-file-scope-variables` config-bundling plugin, mirrored (same
// two byte-identical copies as externalizeDepsPlugin above). The build defines
// `__dirname` / `__filename` / `import.meta.url` (and `import.meta.dirname` /
// `.filename`) as `__vite_injected_original_*` identifiers, and this onLoad
// prepends per-file consts carrying THAT file's real location, so a bundle
// imported from the cache dir still sees the original paths -- for every
// inlined file, not just the config entry. esbuild scopes and renames the
// consts per module, so each file keeps its own values.
const CONFIG_BUNDLE_DEFINES = {
  __dirname: "__vite_injected_original_dirname",
  __filename: "__vite_injected_original_filename",
  "import.meta.url": "__vite_injected_original_import_meta_url",
  "import.meta.dirname": "__vite_injected_original_dirname",
  "import.meta.filename": "__vite_injected_original_filename",
};
function injectFileScopeVariablesPlugin() {
  return {
    name: "inject-file-scope-variables",
    setup(build) {
      build.onLoad({ filter: /\.[cm]?[jt]sx?$/, namespace: "file" }, (args) => {
        const contents = readFileSync(args.path, "utf8");
        const inject =
          `const __vite_injected_original_dirname = ${JSON.stringify(dirname(args.path))};` +
          `const __vite_injected_original_filename = ${JSON.stringify(args.path)};` +
          `const __vite_injected_original_import_meta_url = ${JSON.stringify(pathToFileURL(args.path).href)};`;
        let code;
        if (contents.startsWith("#!")) {
          const nl = contents.indexOf("\n");
          code = nl === -1 ? contents + "\n" + inject : contents.slice(0, nl + 1) + inject + contents.slice(nl + 1);
        } else {
          code = inject + contents;
        }
        const loader = args.path.endsWith(".tsx")
          ? "tsx"
          : /\.(?:ts|mts|cts)$/.test(args.path)
            ? "ts"
            : args.path.endsWith(".jsx")
              ? "jsx"
              : "js";
        return { contents: code, loader };
      });
    },
  };
}

async function loadViteConfig(configPath) {
  const appRoot = initial.config?.root ?? process.cwd();
  const req = createRequire(appRoot + "/package.json");
  try {
    const vite = await import(req.resolve("vite"));
    if (typeof vite.loadConfigFromFile === "function") {
      const loaded = await vite.loadConfigFromFile(
        { command: env.command, mode: env.mode },
        configPath,
        appRoot,
      );
      if (loaded && loaded.config) return loaded.config;
    }
  } catch (e) {
    process.stderr.write(`${OJ}${_ojTTY ? "" : ":"} vite.loadConfigFromFile unavailable (${e}); bundling config directly\n`);
  }
  let mod;
  if (/\.(ts|tsx|mts|cts)$/.test(configPath)) {
    const esbuild = await import(req.resolve("esbuild"));
    const result = await esbuild.build({
      entryPoints: [configPath],
      bundle: true,
      platform: "node",
      format: "esm",
      // Node's resolver, not esbuild's bundler defaults: `conditions` pins
      // "node" (import/require still apply per kind) and drops esbuild's
      // implicit "module" condition; `mainFields` skips "module" as Node does.
      conditions: ["node"],
      mainFields: ["main"],
      plugins: [externalizeDepsPlugin(), injectFileScopeVariablesPlugin()],
      write: false,
      sourcemap: false,
      logLevel: "silent",
      absWorkingDir: appRoot,
      define: CONFIG_BUNDLE_DEFINES,
    });
    // A unique path per process: the config extractor bundles the same config
    // into this directory concurrently at boot, and two writers on one filename
    // made either importer see a truncated bundle.
    const out = join(
      dirname(fileURLToPath(import.meta.url)),
      `oj-vite-config-${process.pid}-${Math.random().toString(36).slice(2)}.tmp.mjs`,
    );
    writeFileSync(out, result.outputFiles[0].text);
    try {
      mod = await import(pathToFileURL(out).href);
    } finally {
      try { unlinkSync(out); } catch {}
    }
  } else {
    mod = await import(pathToFileURL(configPath).href);
  }
  return typeof mod.default === "function" ? await mod.default(env) : mod.default;
}

const OJ_NATIVE_PLUGIN_NAMES = new Set([
  "vite:react-babel",
  "vite:react-refresh",
  "vite:react-swc",
  "vite:react-swc:resolve-runtime",
  // rolldown-vite's @vitejs/plugin-react does its client Fast Refresh wrapping by
  // returning the rolldown builtin `builtin:vite-react-refresh-wrapper` from this
  // plugin's `applyToEnvironment`. oj already reimplements React refresh natively
  // (its own preamble + refresh runtime), so this is redundant; left active the
  // builtin's transform runs and throws "Missing field `moduleType`" (oj drives
  // transform hooks with Vite's { ssr } options, not the builtin's rolldown
  // { moduleType } extra args), which 500s the client entry and stops the browser
  // from ever hydrating. Skipping the parent here removes it before
  // applyToEnvironment can introduce the builtin.
  "vite:react:refresh-wrapper",
]);

// Dev-tooling plugins oj cannot host: they drive a full Vite dev server (ws
// error overlay, file watcher, a worker running tsc/eslint) that oj does not
// provide, and they have no effect on the served or built output. Left in, the
// worker they spawn throws asynchronously; skip them outright.
const OJ_UNSUPPORTED_PLUGIN_NAMES = new Set(["vite-plugin-checker"]);

let plugins = [];
let allPlugins = [];
// The React-family plugins oj reimplements natively. They run no hooks here but
// stay visible in the resolved `config.plugins` (Vite lists every applied
// plugin), so compat probes for vite:react-babel / vite:react-swc find them.
let nativePlugins = [];
const enforceRank = (p) => (p.enforce === "pre" ? -1 : p.enforce === "post" ? 1 : 0);
try {
  let list;
  let userConfig = null;
  if (initial.pluginsFormat === "vite") {
    const cfg = await loadViteConfig(pluginsPath);
    userConfig = cfg && typeof cfg === "object" ? cfg : null;
    userViteConfig = userConfig;
    userResolveAlias = cfg?.resolve?.alias ?? null;
    list = await Promise.all((cfg?.plugins ?? []).flat(Infinity));
  } else {
    const mod = await import(pathToFileURL(pluginsPath).href);
    list = mod.default ?? mod.plugins ?? [];
  }
  plugins = (Array.isArray(list) ? list : [list]).filter(Boolean);
  allPlugins = plugins;
  nativePlugins = plugins.filter((p) => OJ_NATIVE_PLUGIN_NAMES.has(p && p.name));
  plugins = plugins.filter((p) => !OJ_NATIVE_PLUGIN_NAMES.has(p && p.name));
  plugins = plugins.filter((p) => {
    if (OJ_UNSUPPORTED_PLUGIN_NAMES.has(p && p.name)) {
      process.stderr.write(
        `${OJ} plugin host: skipping unsupported plugin "${p.name}" (dev-only tooling oj does not host; your app's output is unaffected)\n`,
      );
      return false;
    }
    return true;
  });
  // Vite (config.ts filterPlugin): `apply({ ...config, mode }, configEnv)` where
  // config is the user's loaded config file merged with the inline config. oj's
  // values (root, base, define, server) overlay the file's.
  const { plugins: _userPlugins, ...userConfigRest } = userConfig ?? {};
  const applyConfig = { ...mergeConfigLite(userConfigRest, initial.config ?? {}), mode: env.mode };
  plugins = plugins.filter((p) => {
    if (p.apply == null) return true;
    try {
      if (typeof p.apply === "function") return !!p.apply(applyConfig, env);
      return p.apply === env.command;
    } catch (e) {
      if (ojStartMode) return true;
      throw e;
    }
  });
  plugins.sort((a, b) => enforceRank(a) - enforceRank(b));
} catch (e) {
  process.stderr.write(`${OJ} plugin host: failed to load ${pluginsPath}: ${(e && e.stack) || e}\n`);
}
initStage("plugins-loaded");

let rpcCounter = 1;
const rpcPending = new Map();
function ctxRpc(method, args) {
  const rpc = rpcCounter++;
  return new Promise((resolve, reject) => {
    rpcPending.set(rpc, { resolve, reject });
    ctl({ rpc, method, args });
  });
}

let emitCounter = 0;
const emitted = [];
// Chunks a plugin asks oj to emit via `this.emitFile({ type: "chunk" })`.
// The reference id is minted here and returned synchronously (Rollup's emitFile
// is sync); the chunk is forwarded to rolldown as a build root by the Rust side,
// and its final hashed name is seeded back into `chunkFileNames` before
// generateBundle so `this.getFileName(refId)` resolves.
const chunkFileNames = new Map();
const transformEmitStore = new AsyncLocalStorage();

// A minimal stand-in for Vite's `vite:css-post` plugin. UnoCSS (and similar)
// look up a plugin named "vite:css-post" in `config.plugins` during their build
// hook and call its `transform` with the CSS they generate, to route it into
// the output. oj collects that CSS here and folds it into the build stylesheet
// (see getPluginCss). Only relevant fields are implemented.
const ojPluginCss = [];
const cssPostShim = {
  name: "vite:css-post",
  transform: {
    handler(code, id) {
      if (typeof code === "string" && code.length > 0) {
        ojPluginCss.push({ id: typeof id === "string" ? id : "", css: code });
      }
      return null;
    },
  },
};

const moduleInfoCache = new Map();

// Vite reports a failing hook as `[plugin:name] message` followed by the module
// id with `line:column` and the code frame (pluginContainer's formatError), and
// fails the request/build. Attach the same so the overlay and the terminal say
// which plugin broke on which file instead of silently serving the raw source.
function decoratePluginError(e, plugin, id) {
  const err = e instanceof Error ? e : new Error(typeof e === "string" ? e : (e && e.message) || String(e));
  if (e && typeof e === "object" && !(e instanceof Error)) Object.assign(err, e);
  if (err.ojDecorated) return err;
  const name = (plugin && plugin.name) || "unknown";
  const loc = err.loc && typeof err.loc === "object" ? err.loc : null;
  const line = loc ? loc.line : err.line;
  const column = loc ? loc.column : err.column;
  const file = (loc && loc.file) || err.id || id || "";
  let msg = `[plugin:${name}] ${err.message}`;
  if (file) msg += `\n${file}${line != null ? `:${line}${column != null ? `:${column}` : ""}` : ""}`;
  if (typeof err.frame === "string" && err.frame) msg += `\n${err.frame}`;
  err.message = msg;
  err.plugin = name;
  err.ojDecorated = true;
  err.stack = msg;
  return err;
}

const watchedFiles = new Set();
const transformWatchStore = new AsyncLocalStorage();
// Per-transform map of oj-resolved imports (spec -> id) the Rust side computes and
// passes in, so a plugin's `this.resolve` during transform is a local lookup rather
// than a host round-trip. ctx.resolve consults it first.
const transformResolveStore = new AsyncLocalStorage();
const seenIds = new Set();

// this.parse is synchronous in Rollup, so resolve Vite's parseAst (re-exported
// from Rollup) once at startup; AST-aware plugins call it inside transform.
let viteParseAst = null;
// Vite's plugin context meta carries `viteVersion` (pluginContainer's
// basePluginContextMeta); plugins feature-detect on it. The app's own Vite
// reports its version; without one, the Vite line oj tracks.
let viteVersion = "8.0.0";
try {
  const _root = initial.config?.root ?? process.cwd();
  const _vite = await import(createRequire(_root + "/package.json").resolve("vite"));
  if (typeof _vite.parseAst === "function") viteParseAst = _vite.parseAst;
  if (typeof _vite.version === "string") viteVersion = _vite.version;
} catch {}

// Vite logs a hook's this.warn through the logger with the plugin's name
// (`warning: <message>` then `  Plugin: <name>`, buildErrorMessage) so the
// terminal says which plugin spoke. The per-plugin ctx (ctxFor) sets _plugin.
function pluginLogMessage(level, raw, plugin) {
  const log = typeof raw === "function" ? raw() : raw;
  const msg = typeof log === "string" ? log : (log && log.message) || String(log);
  const name = (plugin && plugin.name) || (log && typeof log === "object" && log.plugin) || null;
  return `${OJ} ${level}: ${msg}${name ? `\n  Plugin: ${name}` : ""}\n`;
}

// Rollup's ModuleInfo shape (the fields Vite's pluginContainer exposes), so
// moduleParsed / getModuleInfo readers find `meta`, `importers`, `isEntry`.
function makeModuleInfo(id, code, extra) {
  return {
    id,
    code,
    ast: null,
    meta: {},
    importers: [],
    importedIds: [],
    dynamicImporters: [],
    dynamicallyImportedIds: [],
    importedIdResolutions: [],
    dynamicallyImportedIdResolutions: [],
    exports: null,
    exportedBindings: null,
    hasDefaultExport: null,
    isEntry: false,
    isExternal: false,
    isIncluded: null,
    moduleSideEffects: true,
    syntheticNamedExports: false,
    attributes: {},
    ...(extra ?? {}),
  };
}
// Vite's `_updateModuleInfo`: a load/transform result's `meta` merges into the
// module's info (and moduleSideEffects/syntheticNamedExports replace).
function updateModuleInfo(id, result) {
  if (!result || typeof result !== "object") return;
  let info = moduleInfoCache.get(id);
  if (!info) moduleInfoCache.set(id, (info = makeModuleInfo(id, typeof result.code === "string" ? result.code : "")));
  if (result.meta && typeof result.meta === "object") info.meta = { ...(info.meta ?? {}), ...result.meta };
  if (result.moduleSideEffects != null) info.moduleSideEffects = result.moduleSideEffects;
  if (result.syntheticNamedExports != null) info.syntheticNamedExports = result.syntheticNamedExports;
}

const ctx = {
  environment,
  meta: { viteVersion, rollupVersion: "4.0.0", rolldownVersion: "1.0.0", watchMode: true, framework: "oj" },
  parse: (code, opts) => (viteParseAst ? viteParseAst(code, opts) : {}),
  get pluginName() {
    return (this && this._plugin && this._plugin.name) || "";
  },
  debug() {},
  info(m) {
    process.stderr.write(pluginLogMessage("info", m, this && this._plugin));
  },
  warn(m) {
    process.stderr.write(pluginLogMessage("warning", m, this && this._plugin));
  },
  error: (m) => {
    throw typeof m === "string" ? new Error(m) : m;
  },
  async resolve(source, importer, options) {
    if (source === "/@react-refresh") return { id: "/@oj/refresh-runtime.js" };
    // Vite (pluginContainer ctx.resolve): the container's resolveId chain runs
    // first, and only then Vite's own resolver. A sibling plugin's virtual id
    // resolves here instead of falling through to oj's disk resolver and coming
    // back null. `attributes` / `custom` / `isEntry` travel to the chain like
    // Vite's do, and the resolved object (external, meta, moduleSideEffects)
    // comes back whole.
    // `skipSelf` mirrors Vite's cumulative `skipCalls`: each nested resolve
    // appends { id, importer, plugin } to the chain (or marks an identical
    // re-issued call `called`), so two plugins that both this.resolve the same
    // id with skipSelf cannot re-enter each other forever. A skip covering only
    // the direct caller resets the exclusion at every hop.
    const self = (this && this._plugin) || null;
    const ambient = (this && this._resolveSkipCalls) || null;
    let skipCalls = ambient;
    if (!options || options.skipSelf !== false) {
      const prior = ambient ? ambient.findIndex((c) => c.id === source && c.importer === importer && c.plugin === self) : -1;
      if (prior !== -1) {
        skipCalls = ambient.slice();
        skipCalls[prior] = { ...skipCalls[prior], called: true };
      } else if (self) {
        skipCalls = ambient ? [...ambient, { id: source, importer, plugin: self }] : [{ id: source, importer, plugin: self }];
      }
    }
    const viaPlugin = await resolveIdFull(source, importer, {
      skipCalls,
      attributes: options && options.attributes,
      custom: options && options.custom,
      isEntry: !!(options && options.isEntry),
    });
    if (viaPlugin != null) return viaPlugin;
    const map = transformResolveStore.getStore();
    if (map && map.has(source)) {
      const id = map.get(source);
      return id == null ? null : { id };
    }
    const id = await ctxRpc("resolve", [source, importer ?? ""]);
    return id == null ? null : { id };
  },
  emitFile(file) {
    if (file == null) {
      throw new Error("oj: this.emitFile requires a file descriptor");
    }
    if (file.type === "chunk") {
      if (!file.id) {
        throw new Error("oj: this.emitFile({ type: 'chunk' }) requires an id");
      }
      const referenceId = `oj-chunk-${emitCounter++}`;
      const rec = {
        referenceId,
        id: file.id,
        name: file.name ?? null,
        fileName: file.fileName ?? null,
      };
      const bucket = transformEmitStore.getStore();
      if (bucket) bucket.push(rec);
      return referenceId;
    }
    if (file.type && file.type !== "asset") {
      throw new Error("oj: this.emitFile supports { type: 'asset' | 'chunk' }");
    }
    const fileName = file.fileName ?? `assets/${file.name ?? `asset-${emitCounter}`}`;
    const referenceId = `oj-ref-${emitCounter++}`;
    emitted.push({ referenceId, fileName, source: String(file.source ?? "") });
    return referenceId;
  },
  getFileName(referenceId) {
    if (chunkFileNames.has(referenceId)) return chunkFileNames.get(referenceId);
    const f = emitted.find((e) => e.referenceId === referenceId);
    if (!f) throw new Error(`oj: unknown emit reference ${referenceId}`);
    return f.fileName;
  },
  async load(options) {
    const id = typeof options === "string" ? options : options.id;
    // Vite (pluginContainer ctx.load): container.load runs the plugins' load
    // chain, then transform, and hands back the module info. Only when no plugin
    // serves the id does the source come from disk (oj's native compile).
    let info = null;
    const fromPlugin = await loadFull(id);
    if (fromPlugin != null) {
      const outerWatch = transformWatchStore.getStore();
      updateModuleInfo(id, fromPlugin);
      const out = JSON.parse(await transform(fromPlugin.code, id, null));
      if (outerWatch) for (const f of out.watchFiles) outerWatch.add(f);
      info = moduleInfoCache.get(id) ?? makeModuleInfo(id, out.code);
      info.code = out.code;
    } else {
      const raw = await ctxRpc("moduleInfo", [id]);
      info = raw ? makeModuleInfo(raw.id ?? id, raw.code ?? "", raw) : null;
    }
    if (info) {
      moduleInfoCache.set(info.id, info);
      seenIds.add(info.id);
    }
    return info;
  },
  getModuleInfo(id) {
    return moduleInfoCache.get(typeof id === "string" ? id : id.id) ?? null;
  },
  addWatchFile(id) {
    if (!id) return;
    watchedFiles.add(String(id));
    const bucket = transformWatchStore.getStore();
    if (bucket) bucket.add(String(id));
  },
  getModuleIds() {
    return seenIds.values();
  },
};

// Vite gives every plugin its own context object whose `_plugin` is the plugin
// itself (that is how ctx.resolve knows whom to skip). Derive a per-plugin view
// over the shared ctx so `this._plugin` is set inside the hooks.
const pluginCtxs = new WeakMap();
function ctxFor(p) {
  if (!p || typeof p !== "object") return ctx;
  let c = pluginCtxs.get(p);
  if (!c) {
    c = Object.create(ctx, { _plugin: { value: p } });
    pluginCtxs.set(p, c);
  }
  return c;
}

// Vite's ResolveIdContext carries the chain's skipCalls, so a nested
// ctx.resolve accumulates onto the whole chain instead of starting over.
function resolveHookCtx(p, skipCalls) {
  const base = ctxFor(p);
  if (!skipCalls) return base;
  return Object.create(base, { _resolveSkipCalls: { value: skipCalls } });
}


let pluginConfigDelta = {};
async function runConfigHooks() {
  userResolvedViteConfig = await loadUserResolvedViteConfig();
  initStage("user-config-resolved");
  let config = initial.config ?? {};
  if (userResolvedViteConfig) {
    // The user's resolved config is the base; oj's own values (root, base,
    // define, server, environments) overlay it; the plugin list is replaced below.
    const { plugins: _fresh, ...base } = userResolvedViteConfig;
    config = mergeConfigLite(base, config);
  }
  // Coerce experimental.bundledDev off BEFORE the config/configResolved hooks
  // run, so TanStack's start-manifest-plugin emits the standard dev client entry
  // (not `/assets/index.js`) and any bundled-dev middleware early-returns.
  coerceBundledDevOff(config);
  // Vite hands the config hook the user config, whose `plugins` is the flat
  // plugin array; plugins like @crxjs read `config.plugins` to find sibling
  // plugins. Use the apply-filtered active set (not allPlugins) so command-
  // inappropriate plugins are absent, matching Vite's resolved config — e.g.
  // `@crxjs`'s serve-only `crx:hmr` must not appear during a build, or crx calls
  // its `transformCrxManifest` (which reads an unset `config`) and throws.
  // The exposed plugin array: the apply-filtered active set plus the css-post
  // shim. Pinned across the config-hook merges below so mergeConfigLite (which
  // concatenates arrays) can't accumulate it into duplicates.
  // The natively reimplemented React plugins stay listed (hooks unrun), in
  // enforce order like Vite's resolved plugin array.
  const configPlugins = plugins.concat(nativePlugins).sort((a, b) => enforceRank(a) - enforceRank(b));
  if (!configPlugins.some((p) => p && p.name === "vite:css-post")) {
    configPlugins.push(cssPostShim);
  }
  config.plugins = configPlugins;
  for (const { p, fn } of pluginsWithHook("config")) {
    initStage(`config:${p.name ?? "?"}`);
    try {
      const partial = await fn.call(ctxFor(p), config, env);
      if (partial) {
        config = mergeConfigLite(config, partial);
        // What the plugins themselves contributed (Vite merges it into the
        // resolved config); Rust asks for it via getPluginConfig so values oj
        // applies natively (define) reach the compile too.
        pluginConfigDelta = mergeConfigLite(pluginConfigDelta, partial);
      }
    } catch (e) {
      if (!ojStartMode) throw e;
      process.stderr.write(`${OJ} plugin host: config(${p.name ?? "?"}) skipped: ${(e && e.message) || e}\n`);
    }
    // Keep the plugin array stable regardless of what a config hook returned.
    config.plugins = configPlugins;
  }
  // Vite runs `configEnvironment(name, options, env)` for every environment after
  // the config hooks and merges what it returns into that environment's options.
  const envNames = Object.keys(config.environments ?? {});
  for (const n of ["client", "ssr"]) if (!envNames.includes(n)) envNames.push(n);
  const configEnvHooks = pluginsWithHook("configEnvironment");
  if (configEnvHooks.length) {
    config.environments = { ...(config.environments ?? {}) };
    for (const name of envNames) {
      config.environments[name] = config.environments[name] ?? {};
      const opts = {
        ...env,
        isSsrBuild: name !== "client" && env.command === "build",
        isPreview: false,
        isSsrTargetWebworker: false,
      };
      for (const { p, fn } of configEnvHooks) {
        initStage(`configEnvironment:${p.name ?? "?"}`);
        try {
          const r = await fn.call(ctx, name, config.environments[name], opts);
          if (r) config.environments[name] = mergeConfigLite(config.environments[name], r);
        } catch (e) {
          if (!ojStartMode) throw e;
          process.stderr.write(`${OJ} plugin host: configEnvironment(${p.name ?? "?"}) skipped: ${(e && e.message) || e}\n`);
        }
      }
    }
  }
  resolvedConfig = withResolvedDefaults(config);
  resolvedConfig.plugins = configPlugins;
  for (const { p, fn } of pluginsWithHook("configResolved")) {
    initStage(`configResolved:${p.name ?? "?"}`);
    try {
      await fn.call(ctx, resolvedConfig);
    } catch (e) {
      if (!ojStartMode) throw e;
      process.stderr.write(`${OJ} plugin host: configResolved(${p.name ?? "?"}) skipped: ${(e && e.message) || e}\n`);
    }
  }
}
await runConfigHooks();
initStage("config-hooks-done");
// Vite's environment carries the resolved config (per-environment overrides
// merged), its logger and getTopLevelConfig(); applyToEnvironment and every
// hook's this.environment read them.
environment.config =
  envName === "client"
    ? resolvedConfig
    : withResolvedDefaults(mergeConfigLite(resolvedConfig, (resolvedConfig.environments ?? {})[envName] ?? {}));
environment.config.consumer = environment.config.consumer ?? (envName === "client" ? "client" : "server");
environment.logger = resolvedConfig.logger;
environment.getTopLevelConfig = () => resolvedConfig;

// Vite (plugin.ts resolveEnvironmentPlugins): `applyToEnvironment` is awaited;
// a falsy result drops the plugin, `true` keeps it, and a plugin or plugin
// array replaces it with what it returned (perEnvironmentPlugin), whose
// config-phase hooks are ignored with a warning.
{
  const ignoredEnvironmentPluginHooks = ["config", "configEnvironment", "configureServer", "configResolved"];
  const next = [];
  for (const p of plugins) {
    if (typeof p.applyToEnvironment !== "function") {
      next.push(p);
      continue;
    }
    let applied;
    initStage(`applyToEnvironment:${p.name ?? "?"}`);
    try {
      applied = await p.applyToEnvironment(environment);
    } catch (e) {
      process.stderr.write(
        `${OJ} plugin host: applyToEnvironment(${p.name ?? "?"}) threw; keeping the plugin active: ${(e && e.message) || e}\n`,
      );
      next.push(p);
      continue;
    }
    if (!applied) continue;
    if (applied === true) {
      next.push(p);
      continue;
    }
    const returned = (await Promise.all((Array.isArray(applied) ? applied : [applied]).flat(Infinity)))
      .flat(Infinity)
      .filter(Boolean);
    for (const ap of returned) {
      const ignored = ignoredEnvironmentPluginHooks.filter((hook) => ap[hook]);
      if (ignored.length > 0) {
        process.stderr.write(
          `${OJ} plugin host: Plugin "${ap.name}" defines Vite-specific hooks (${ignored.join(", ")}) in a plugin returned from applyToEnvironment. These hooks will be ignored.\n`,
        );
      }
    }
    next.push(...returned);
  }
  plugins = next;
}
process.stderr.write(
  `${OJ} plugin host: ${plugins.length} plugin(s) active for ${env.command}: ${plugins.map((p) => `${p.name}[${p.enforce ?? "-"}]`).join(",")}\n`,
);

// Events for the Rust server (not the browser): a plugin invalidating a module
// through server.moduleGraph, or asking for server.restart().
function ojServerEvent(action, data) {
  ctl({ ojServer: { action, ...(data ?? {}) } });
}

// A ModuleGraph stand-in for plugins that reach into `server.moduleGraph` (or an
// environment's) from configureServer / hotUpdate. Vite's getModuleById returns
// undefined for ids it has never seen; oj's graph lives in Rust, so the host
// knows an id when a hook has seen it (transform / load / this.load) or when it
// names a real file (oj serves those without telling the host), and answers
// undefined for anything else (an unknown virtual id) so plugins guarding on it
// take the same branch as under Vite. `invalidateModule` tells oj to drop the
// module's compiled output and propagate an update, as Vite's graph does.
function knownModuleId(id) {
  if (seenIds.has(id)) return true;
  const file = String(id).split("?", 1)[0];
  if (!file || file.startsWith("\0") || !isAbsolute(file)) return false;
  try {
    return statSync(file).isFile();
  } catch {
    return false;
  }
}
function createModuleGraph() {
  const graphNodes = new Map();
  const fileToModulesMap = new Map();
  function moduleNode(id) {
    let n = graphNodes.get(id);
    if (!n) {
      const file = String(id).split("?", 1)[0];
      n = {
        id,
        url: id,
        file,
        type: "js",
        info: null,
        importers: new Set(),
        importedModules: new Set(),
        acceptedHmrDeps: new Set(),
        acceptedHmrExports: null,
        isSelfAccepting: false,
        lastHMRTimestamp: 0,
        lastInvalidationTimestamp: 0,
        transformResult: null,
        ssrTransformResult: null,
        ssrModule: null,
        ssrError: null,
      };
      graphNodes.set(id, n);
      let byFile = fileToModulesMap.get(file);
      if (!byFile) fileToModulesMap.set(file, (byFile = new Set()));
      byFile.add(n);
    }
    return n;
  }
  return {
    getModuleById: (id) => (id == null || !knownModuleId(String(id)) ? undefined : moduleNode(String(id))),
    getModuleByUrl: async (url) => (url == null || !knownModuleId(String(url)) ? undefined : moduleNode(String(url))),
    getModulesByFile: (file) =>
      fileToModulesMap.get(String(file)) ?? (knownModuleId(String(file)) ? new Set([moduleNode(String(file))]) : undefined),
    // Vite creates the node on demand here (used by plugins that pre-seed urls).
    ensureEntryFromUrl: async (url) => moduleNode(String(url)),
    invalidateModule(mod) {
      if (!mod) return;
      mod.lastInvalidationTimestamp = Date.now();
      mod.transformResult = null;
      mod.ssrTransformResult = null;
      ojServerEvent("invalidate", { id: mod.id });
    },
    invalidateAll() {
      for (const m of graphNodes.values()) m.transformResult = null;
      ojServerEvent("invalidateAll");
    },
    onFileChange(file) {
      ojServerEvent("invalidate", { id: String(file) });
    },
    urlToModuleMap: graphNodes,
    idToModuleMap: graphNodes,
    fileToModulesMap,
  };
}
const moduleGraph = createModuleGraph();

const wsListeners = new Map();
function ojWsSend(event, data) {
  ctl({ ojWs: { event, data: data ?? null } });
}
const wsApi = {
  on(event, cb) {
    let a = wsListeners.get(event);
    if (!a) wsListeners.set(event, (a = new Set()));
    a.add(cb);
  },
  off(event, cb) {
    wsListeners.get(event)?.delete(cb);
  },
  send(a, b) {
    if (typeof a === "string") ojWsSend(a, b);
    else if (a && typeof a === "object" && typeof a.event === "string") ojWsSend(a.event, a.data);
    else ojWsSend(null, a);
  },
};

// Vite's `this.environment` is the DevEnvironment: alongside name/mode/config it
// carries the moduleGraph, hot channel and plugin list (pluginContainer
// MinimalPluginContext); the config/logger were attached once config hooks ran.
environment.moduleGraph = moduleGraph;
environment.hot = wsApi;
environment.plugins = plugins;

// `server.ws.on("connection", cb)` (Vite: the raw ws server's connection event,
// per client). oj's Rust listener accepts the socket; it tells the host, which
// hands listeners a socket whose `send` goes back out through oj's channel.
function wsConnection() {
  const listeners = wsListeners.get("connection");
  if (!listeners) return null;
  const socket = {
    readyState: 1,
    OPEN: 1,
    send(raw) {
      let parsed = null;
      try {
        parsed = typeof raw === "string" ? JSON.parse(raw) : raw;
      } catch {}
      if (parsed && parsed.type === "custom" && typeof parsed.event === "string") ojWsSend(parsed.event, parsed.data);
      else ojWsSend(null, parsed ?? raw);
    },
    on() {},
    once() {},
    off() {},
    close() {},
  };
  const req = { url: "/", headers: {}, method: "GET" };
  for (const cb of [...listeners]) {
    try {
      cb(socket, req);
    } catch (e) {
      process.stderr.write(`${OJ} ws.on(connection) handler failed: ${(e && e.stack) || e}\n`);
    }
  }
  return null;
}

let middlewarePort = null;
// Whether buildEnvironments produced real runner-backed Vite DevEnvironments
// (the Environment-API path, activated by any plugin or config declaring an
// `environments.<name>.dev.createEnvironment` factory, e.g. the Cloudflare
// plugin); reported through getServeInfo, which Rust reads to gate its
// SSR-runner handling.
let runnerEnvironmentsBuilt = false;
// The ViteDevServer stand-in handed to configureServer; hotUpdate/handleHotUpdate
// contexts carry it as `server` (plugins call server.ws.send / moduleGraph on it).
let devServer = null;
let appVite = null;
async function loadAppVite() {
  if (appVite) return appVite;
  try {
    const root = (resolvedConfig && resolvedConfig.root) || (initial.config && initial.config.root) || process.cwd();
    appVite = await import(createRequire(root + "/package.json").resolve("vite"));
  } catch (e) {
    process.stderr.write(`${OJ} plugin host: could not load app vite: ${(e && e.message) || e}\n`);
  }
  return appVite;
}

// Build real Vite DevEnvironments from the app's installed Vite so plugins that
// use the Environment API (e.g. @cloudflare/vite-plugin, which subclasses
// vite.DevEnvironment) run. oj does not depend on Vite; it loads the app's copy.
async function buildEnvironments(server) {
  const vite = await loadAppVite();
  if (!vite || typeof vite.resolveConfig !== "function") return undefined;
  const root = (resolvedConfig && resolvedConfig.root) || (initial.config && initial.config.root) || process.cwd();
  // Real DevEnvironments need a real resolved Vite config (root, cacheDir,
  // resolve.builtins, per-env dev.createEnvironment factories). Hand-faking it
  // is endless whack-a-mole, so ask the app's Vite to resolve it once.
  let rc;
  try {
    // configFile:undefined lets Vite resolve fresh plugin instances. Reusing
    // oj's instances (configFile:false + plugins) corrupts stateful plugins
    // (e.g. @cloudflare/vite-plugin's virtual:cloudflare/export-types) when
    // their config hooks run a second time, so we accept a second resolve. The
    // environments bind to these fresh instances; oj drives its own instances
    // in configureServer, and the two agree because both resolve from `root`.
    hostPhase("cf: resolveConfig begin");
    initStage("cf:resolveConfig");
    rc = await vite.resolveConfig({ root, configFile: undefined, mode: environment.mode }, "serve", "development", "development");
    hostPhase("cf: resolveConfig done");
    initStage("cf:resolveConfig-done");
    // The fresh resolve reads the app's vite.config again, so bundledDev is back
    // on here (and Vite has stamped environments.client.isBundled). Coerce it off
    // BEFORE the environments are built off `rc`: the DevEnvironment constructor
    // makes the client env bundled from these fields, and the manifest plugin
    // bound to these environments reads them at load time.
    coerceBundledDevOff(rc);
  } catch (e) {
    process.stderr.write(`${OJ} plugin host: vite.resolveConfig failed: ${(e && e.message) || e}\n`);
    return undefined;
  }
  server.config = rc;
  // import-analysis pre-warms a module's static imports only when the
  // environment's dev.preTransformRequests is on, and Vite defaults it off for
  // server consumers; without it the module runner walks its graph one serial
  // fetchModule at a time. Turn it on for server environments unless the user
  // chose a value (the environments read this resolved object live).
  for (const [name, envOpts] of Object.entries(rc.environments || {})) {
    if (!envOpts) continue;
    const consumer = envOpts.consumer ?? (name === "client" ? "client" : "server");
    if (consumer === "client" || userSetPreTransformRequests(name)) continue;
    if (envOpts.dev && typeof envOpts.dev === "object") envOpts.dev.preTransformRequests = true;
    else envOpts.dev = { preTransformRequests: true };
  }
  const environments = {};
  // Vite's createServer creates and inits every environment in parallel.
  await Promise.all(Object.entries(rc.environments || {}).map(async ([name, envOpts]) => {
    let ei;
    try {
      const factory = envOpts && envOpts.dev && envOpts.dev.createEnvironment;
      ei = factory
        ? await factory(name, rc, { ws: server.ws })
        : new vite.DevEnvironment(name, rc, { hot: true, transport: server.ws });
      hostPhase(`cf: createEnvironment(${name}) done`);
      initStage(`cf:createEnvironment:${name}`);
    } catch (e) {
      process.stderr.write(`${OJ} plugin host: createEnvironment(${name}) failed: ${(e && e.message) || e}\n`);
      return;
    }
    environments[name] = ei;
    try {
      if (ei && typeof ei.init === "function") {
        await ei.init({ watcher: server.watcher });
        hostPhase(`cf: init(${name}) done`);
        initStage(`cf:init:${name}`);
      }
    } catch (e) {
      process.stderr.write(`${OJ} plugin host: env.init(${name}) failed: ${(e && e.message) || e}\n`);
    }
  }));
  return environments;
}

function hostPhase(label) {
  if (process.env.OJ_BOOT_PHASES) process.stderr.write(`[oj-phase] ${Date.now()} ${label}\n`);
}

function userSetPreTransformRequests(name) {
  for (const cfg of [userViteConfig, initial.config]) {
    if (!cfg) continue;
    if (cfg.environments?.[name]?.dev?.preTransformRequests !== undefined) return true;
    if (cfg.server?.preTransformRequests !== undefined) return true;
  }
  return false;
}

// On a source edit oj POSTs the changes ({path, type} with Vite's watcher event
// types: update | create | delete) to /__oj_invalidate. For each runner-backed
// (real Vite) DevEnvironment this mirrors the core of Vite's
// handleHMRUpdate/updateModules/propagateUpdate: invalidate the changed modules,
// walk the module graph to the nearest accept boundaries and send a targeted
// {type:"update"} so the runner re-fetches only the invalidated chain (the
// Cloudflare plugin's worker entry self-accepts). Propagation dead-ends and
// graphs without the walk API fall back to Vite's full-reload; a throwing
// hotUpdate hook becomes {type:"error"}, as in Vite's hmr().
// (Vite's watcher drives all of this itself; oj drives it since it owns the
// file watcher. Runner-sent vite:invalidate is handled by the DevEnvironment.)
const WATCHER_EVENT = { update: "change", create: "add", delete: "unlink" };
const unmatchedChangeLogged = new Set();
// oj sends each edit twice (an early pre-settle POST and the settled batch),
// while Vite sees ONE chokidar event per FS change: replaying both would run
// watcher.emit and the whole hotUpdate pipeline (hooks, targeted updates, the
// runner's chain re-fetch) twice. Dedup by content identity: skip a change
// whose type AND file content hash match the last processed one — consecutive
// identical content is the same FS change (the early/settled pair), while a
// write landing inside the settle window, a revert, or same-mtime distinct
// writes all hash differently and process. A read error means the file's
// state is unknown: process and forget the entry, never suppress on error.
// Consecutive deletes dedup by type alone.
const lastProcessedChange = new Map(); // file -> { type, contentHash }
// Correctness never depends on an entry existing (a cleared entry only costs
// one extra processing pass), so bound the map instead of growing forever.
const LAST_PROCESSED_MAX = 5000;
function freshChange({ file, type }) {
  let contentHash = null;
  if (type !== "delete") {
    try {
      contentHash = createHash("sha256").update(readFileSync(file)).digest("hex");
    } catch {
      lastProcessedChange.delete(file);
      return true;
    }
  }
  const prev = lastProcessedChange.get(file);
  if (lastProcessedChange.size >= LAST_PROCESSED_MAX) lastProcessedChange.clear();
  lastProcessedChange.set(file, { type, contentHash });
  return !(prev && prev.type === type && prev.contentHash === contentHash);
}
// A "create" for a file some runner-backed module graph already knows is an
// atomic save's recreate half (chokidar's atomic handling reports these as
// change): reclassify to update, so the update-then-create pair dedups into
// one pass and plugins never see a phantom watcher "add" for a live module.
function graphKnowsFile(environments, file) {
  for (const env of Object.values(environments || {})) {
    if (!env || env.__ojStub) continue;
    try {
      const mods = env.moduleGraph?.getModulesByFile?.(file);
      if (mods && mods.size > 0) return true;
    } catch {}
  }
  return false;
}
let invalidateQueue = Promise.resolve();
// Whether a resync is already enqueued (and not yet run): duplicates coalesce
// into it — see the /__oj_invalidate resync branch.
let resyncPending = false;
// The catch-up for a late middleware activation ({resync:true} on
// /__oj_invalidate): edits made while oj had no port to invalidate never
// reached these graphs, and init-time pre-transforms (preTransformRequests)
// may hold their stale results. Invalidate every runner-backed environment's
// whole graph and full-reload, bypassing the per-change dedup (its identity
// map is cleared so nothing is suppressed against pre-resync state).
function resyncEnvironments(environments) {
  lastProcessedChange.clear();
  for (const env of Object.values(environments || {})) {
    if (!env || env.__ojStub) continue;
    try { env.moduleGraph?.invalidateAll?.(); } catch {}
    hotSend(env, { type: "full-reload", path: "*" });
  }
}
async function invalidateEnvironments(environments, watcher, changes) {
  // Re-classify at handling time: the early (pre-settle) send classifies from
  // a raw event, and an atomic-save editor's momentary delete may have been
  // recreated by the time this runs; a stale "delete" would prune a live
  // module from the graph.
  const normalized = changes
    .map(({ path: file, type }) => {
      let t = type || "update";
      file = String(file).replace(/\\/g, "/");
      if (t === "delete" && existsSync(file)) t = "update";
      if (t === "create" && graphKnowsFile(environments, file)) t = "update";
      return { file, type: t };
    })
    .filter(freshChange);
  for (const { file, type } of normalized) {
    try { watcher.emit(WATCHER_EVENT[type], file); } catch {}
  }
  if (!environments) return;
  const timestamp = Date.now();
  const matched = new Set();
  for (const env of Object.values(environments)) {
    // oj's own HMR already covers the stub environments; only runner-backed
    // (real Vite) environments need their graph invalidated and updates sent.
    if (!env || env.__ojStub) continue;
    for (const { file, type } of normalized) {
      try {
        if (await hotUpdateEnvironment(env, file, timestamp, type)) matched.add(file);
      } catch (e) {
        process.stderr.write(`${OJ} plugin host: hot update failed (${env.name}): ${(e && e.stack) || e}\n`);
        hotSend(env, { type: "error", err: prepareErrorPayload(e) });
      }
    }
  }
  // A changed file no runner-backed graph knows is Vite's "no modules matched"
  // (nothing is sent), but here it can also mean the watcher path spells the
  // file differently than the graph keys (a symlink, casing) — make the
  // staleness visible once per file instead of silently serving old modules.
  for (const { file, type } of normalized) {
    if (type === "delete" || matched.has(file) || unmatchedChangeLogged.has(file)) continue;
    unmatchedChangeLogged.add(file);
    process.stderr.write(`${OJ} plugin host: change to ${file} matched no module in any runner environment\n`);
  }
}

// Vite's prepareError: the payload hot.send({type:"error"}) carries.
function prepareErrorPayload(err) {
  const e = err && typeof err === "object" ? err : { message: String(err) };
  return {
    message: stripVTControlCharacters(e.message || String(err)),
    stack: stripVTControlCharacters(e.stack || ""),
    id: e.id,
    frame: stripVTControlCharacters(e.frame || ""),
    plugin: e.plugin,
    pluginCode: e.pluginCode != null ? String(e.pluginCode) : undefined,
    loc: e.loc,
  };
}


function hotSend(env, payload) {
  try {
    if (env.hot && typeof env.hot.send === "function") env.hot.send(payload);
  } catch (e) {
    process.stderr.write(`${OJ} plugin host: hot.send failed (${env.name}): ${(e && e.message) || e}\n`);
  }
}

// Vite's getSortedPluginsByHotUpdateHook: order by the hotUpdate (or legacy
// handleHotUpdate) hook's declared order, cached per environment like Vite.
const sortedHotUpdateCache = new WeakMap();
function sortedHotUpdatePlugins(env) {
  let sorted = sortedHotUpdateCache.get(env);
  if (sorted) return sorted;
  sorted = [];
  let pre = 0, normal = 0, post = 0;
  for (const plugin of env.plugins ?? []) {
    const hook = plugin && (plugin.hotUpdate ?? plugin.handleHotUpdate);
    if (!hook) continue;
    const order = typeof hook === "object" ? hook.order : undefined;
    if (order === "pre") sorted.splice(pre++, 0, plugin);
    else if (order === "post") sorted.splice(pre + normal + post++, 0, plugin);
    else sorted.splice(pre + normal++, 0, plugin);
  }
  sortedHotUpdateCache.set(env, sorted);
  return sorted;
}

// Returns whether the change matched this environment (a module, or a reload
// was sent), so the caller can surface changes no environment knows about.
async function hotUpdateEnvironment(env, file, timestamp, type) {
  const mg = env.moduleGraph;
  if (!mg || typeof mg.getModulesByFile !== "function") {
    hotSend(env, { type: "full-reload" });
    return true;
  }
  // Vite's watcher wiring: watchChange on every event; onFileChange for
  // updates, onFileDelete for deletes.
  if (env.pluginContainer && typeof env.pluginContainer.watchChange === "function") {
    await env.pluginContainer.watchChange(file, { event: type });
  }
  if (type === "delete") {
    if (typeof mg.onFileDelete === "function") mg.onFileDelete(file);
  } else if (type === "update" && typeof mg.onFileChange === "function") {
    mg.onFileChange(file);
  }
  const mods = new Set(mg.getModulesByFile(file) ?? []);
  // A created file may fix imports that previously failed to resolve: retry
  // those modules, as Vite does on "create".
  if (type === "create" && mg._hasResolveFailedErrorModules) {
    for (const m of mg._hasResolveFailedErrorModules) mods.add(m);
  }
  const options = {
    type,
    file,
    timestamp,
    modules: [...mods],
    read: () => readModifiedFile(file),
    server: devServer,
  };
  const context = (env.pluginContainer && env.pluginContainer.minimalContext) || { environment: env };
  for (const plugin of sortedHotUpdatePlugins(env)) {
    if (plugin.hotUpdate) {
      const hook = plugin.hotUpdate;
      const handler = typeof hook === "object" ? hook.handler : hook;
      const filtered = await handler.call(context, options);
      if (filtered) options.modules = [...filtered];
    } else if (env.name === "client" && type === "update") {
      // Legacy handleHotUpdate is a client-only hook in Vite; the mixed module
      // graph its context carries is approximated with the client
      // environment's own modules.
      const hook = plugin.handleHotUpdate;
      const handler = typeof hook === "object" ? hook.handler : hook;
      const filtered = await handler.call(context, {
        file,
        timestamp,
        modules: options.modules,
        read: options.read,
        server: devServer,
      });
      if (filtered) options.modules = [...filtered];
    }
  }
  // No module of this environment is affected: nothing to send, as in Vite
  // ("no modules matched"), except a client html edit which reloads the page.
  if (!options.modules.length) {
    if (file.endsWith(".html") && env.name === "client") {
      hotSend(env, { type: "full-reload", triggeredBy: file, path: "*" });
      return true;
    }
    return mods.size > 0;
  }
  updateModules(env, file, options.modules, timestamp);
  return true;
}

// Vite's updateModules: invalidate each changed module, propagate to accept
// boundaries, send targeted updates, full-reload on a propagation dead end.
function updateModules(env, file, modules, timestamp) {
  const updates = [];
  const invalidatedModules = new Set();
  const traversedModules = new Set();
  let needFullReload = false;
  for (const mod of modules) {
    const boundaries = [];
    const hasDeadEnd = propagateUpdate(mod, traversedModules, boundaries);
    if (typeof env.moduleGraph.invalidateModule === "function") {
      env.moduleGraph.invalidateModule(mod, invalidatedModules, timestamp, true);
    }
    if (needFullReload) continue;
    if (hasDeadEnd) {
      needFullReload = true;
      continue;
    }
    updates.push(...boundaries.map(({ boundary, acceptedVia, isWithinCircularImport }) => ({
      type: `${boundary.type ?? "js"}-update`,
      timestamp,
      path: normalizeHmrUrl(boundary.url),
      acceptedPath: normalizeHmrUrl(acceptedVia.url),
      explicitImportRequired: (boundary.type ?? "js") === "js" ? isExplicitImportRequired(acceptedVia.url) : false,
      isWithinCircularImport,
    })));
  }
  const isClientHtmlChange = file.endsWith(".html") && env.name === "client" && modules.every((m) => m.type !== "js");
  if (needFullReload || isClientHtmlChange) {
    hotSend(env, { type: "full-reload", triggeredBy: file, path: "*" });
    return;
  }
  if (updates.length === 0) return;
  hotSend(env, { type: "update", updates });
}

// Vite's propagateUpdate over EnvironmentModuleNodes: stop at self-accepting
// modules and accepting importers, dead-end (-> full reload) at a root with no
// boundary. Returns true on a dead end.
function propagateUpdate(node, traversedModules, boundaries, currentChain = [node]) {
  if (traversedModules.has(node)) return false;
  traversedModules.add(node);
  // Not analyzed yet (never transformed): nothing imported it, stop quietly.
  if (node.id && node.isSelfAccepting === undefined) return false;
  if (node.isSelfAccepting) {
    boundaries.push({ boundary: node, acceptedVia: node, isWithinCircularImport: isNodeWithinCircularImports(node, currentChain) });
    return false;
  }
  if (node.acceptedHmrExports) {
    boundaries.push({ boundary: node, acceptedVia: node, isWithinCircularImport: isNodeWithinCircularImports(node, currentChain) });
  } else if (!node.importers || !node.importers.size) {
    return true;
  }
  for (const importer of node.importers) {
    const subChain = currentChain.concat(importer);
    if (importer.acceptedHmrDeps && importer.acceptedHmrDeps.has(node)) {
      boundaries.push({ boundary: importer, acceptedVia: node, isWithinCircularImport: isNodeWithinCircularImports(importer, subChain) });
      continue;
    }
    if (node.id && node.acceptedHmrExports && importer.importedBindings) {
      const importedBindingsFromNode = importer.importedBindings.get(node.id);
      if (importedBindingsFromNode && areAllImportsAccepted(importedBindingsFromNode, node.acceptedHmrExports)) continue;
    }
    if (!currentChain.includes(importer) && propagateUpdate(importer, traversedModules, boundaries, subChain)) return true;
  }
  return false;
}

// Vite's isNodeWithinCircularImports: an accepted module inside an import loop
// cannot recover its execution order; the runner full-reloads on the flag.
function isNodeWithinCircularImports(node, nodeChain, currentChain = [node], traversedModules = new Set()) {
  if (traversedModules.has(node)) return false;
  traversedModules.add(node);
  for (const importer of node.importers ?? []) {
    if (importer === node) continue;
    if (nodeChain.includes(importer)) return true;
    if (!currentChain.includes(importer)) {
      if (isNodeWithinCircularImports(importer, nodeChain, currentChain.concat(importer), traversedModules)) return true;
    }
  }
  return false;
}

function areAllImportsAccepted(importedBindings, acceptedExports) {
  for (const binding of importedBindings) if (!acceptedExports.has(binding)) return false;
  return true;
}

// Vite's normalizeHmrUrl: bare/virtual urls travel wrapped as /@id/.
function normalizeHmrUrl(url) {
  if (url[0] !== "." && url[0] !== "/") {
    url = url.startsWith("/@id/") ? url : "/@id/" + url.replace("\0", "__x00__");
  }
  return url;
}

// Vite's isExplicitImportRequired: a non-js, non-css boundary url needs the
// runner client to append ?import.
const knownJsSrcRE = /\.(?:[jt]sx?|m[jt]s|vue|marko|svelte|astro|imba|mdx)(?:$|\?)/;
const knownCssSrcRE = /\.(?:css|less|sass|scss|styl|stylus|pcss|postcss|sss)(?:$|\?)/;
function isExplicitImportRequired(url) {
  const clean = url.split("?", 1)[0].split("#", 1)[0];
  const isJs = knownJsSrcRE.test(url) || (!/\.[^/]+$/.test(clean) && clean[clean.length - 1] !== "/");
  return !isJs && !knownCssSrcRE.test(url);
}

// Vite's `server.httpServer` is the Node http.Server oj's Rust listener stands in
// for: plugins read `address()` for the port and wait on "listening". The
// address is oj's, known at host spawn; "listening" is emitted once
// configureServer has run (later `once("listening")` callers fire immediately,
// as on an already-listening server).
function stubHttpServer() {
  const srvCfg = (resolvedConfig && resolvedConfig.server) || (initial.config && initial.config.server) || {};
  const port = typeof srvCfg.port === "number" ? srvCfg.port : 0;
  const host = typeof srvCfg.host === "string" && srvCfg.host !== "localhost" ? srvCfg.host : "127.0.0.1";
  const s = new EventEmitter();
  let listening = false;
  s.on("listening", () => { listening = true; });
  const wrap = (method) => {
    const orig = s[method].bind(s);
    s[method] = (event, cb) => {
      if (event === "listening" && listening) { cb(); return s; }
      return orig(event, cb);
    };
  };
  wrap("on");
  wrap("once");
  wrap("addListener");
  s.address = () => (port ? { address: host, family: host.includes(":") ? "IPv6" : "IPv4", port } : null);
  s.listen = () => s;
  s.close = (cb) => { if (typeof cb === "function") cb(); return s; };
  Object.defineProperty(s, "listening", { get: () => listening });
  return s;
}

// An oj-backed stand-in for a Vite DevEnvironment (name, config with consumer,
// moduleGraph, hot, plugins, pluginContainer over the host's chains). The client
// stub shares server.moduleGraph; others get their own graph. Marked so oj's
// invalidation of runner-backed environments skips it.
function stubEnvironment(name, server) {
  const isClient = name === "client";
  const base = resolvedConfig ?? {};
  const config = isClient
    ? base
    : withResolvedDefaults(mergeConfigLite(base, (base.environments ?? {})[name] ?? {}));
  // Vite tags each environment's config with its consumer; the merge above
  // inherits the host environment's tag, so set it per environment.
  if (isClient) config.consumer = config.consumer ?? "client";
  else config.consumer = (base.environments ?? {})[name]?.consumer ?? "server";
  const mg = isClient ? moduleGraph : createModuleGraph();
  return {
    __ojStub: true,
    name,
    mode: "dev",
    config,
    logger: base.logger,
    moduleGraph: mg,
    hot: wsApi,
    watcher: server.watcher,
    plugins,
    depsOptimizer: undefined,
    pluginContainer: {
      buildStart: async () => {},
      close: async () => {},
      resolveId: async (source, importer, options) => resolveIdFull(source, importer, options),
      load: async (id) => loadFull(id),
      transform: async (code, id) => ({ code: JSON.parse(await transform(code, id, null)).code }),
    },
    init: async () => {},
    listen: async () => {},
    close: async () => {},
    transformRequest: async () => null,
    warmupRequest: async () => {},
    waitForRequestsIdle: async () => {},
    fetchModule: async () => {
      throw new Error(`oj: server.environments.${name}.fetchModule is not available`);
    },
  };
}

// Vite's `doesProxyContextMatchUrl` (dist server/middlewares/proxy): a `^`
// context is a regex tested against the request url, any other is a prefix.
function doesProxyContextMatchUrl(context, url) {
  return (context[0] === "^" && new RegExp(context).test(url)) || url.startsWith(context);
}

// A proxy `target` can be a string, a URL, or an object with {host/hostname,
// port, protocol, pathname/path/href} (what http-proxy's setupOutgoing reads,
// dist common.js:18190-18234). Normalize any of them to a URL the node pipe can
// dial; returns null for an unusable target.
function pipeTarget(target) {
  if (target instanceof URL) return target;
  if (typeof target === "string") {
    try { return new URL(target); } catch { return null; }
  }
  if (target && typeof target === "object") {
    if (typeof target.href === "string") {
      try { return new URL(target.href); } catch {}
    }
    let protocol = typeof target.protocol === "string" && target.protocol ? target.protocol : "http:";
    if (!protocol.endsWith(":")) protocol += ":";
    const hostOnly = typeof target.host === "string" ? target.host.split(":") : [];
    const hostname = target.hostname || hostOnly[0] || "";
    if (!hostname) return null;
    const port = target.port != null ? String(target.port) : (hostOnly[1] || "");
    const pathname = target.pathname || target.path || "";
    try { return new URL(`${protocol}//${hostname}${port ? ":" + port : ""}${pathname}`); } catch { return null; }
  }
  return null;
}

const targetLabel = (target) =>
  typeof target === "string" ? target : (target && (target.href || target.host)) || "target";

// The single `server.proxy`, hosted in the connect stack the way Vite hosts it
// inside `server.middlewares`. This covers BOTH request origins with one proxy:
// the browser (Rust delegates matched prefixes here) and the worker's OUTBOUND
// fetch, which @cloudflare/vite-plugin routes back through these middlewares.
// It is a faithful `viteProxyMiddleware` (dist 19224): context match, `bypass`,
// a FUNCTION `rewrite` (dropped by the config bridge, but intact here in the
// app's real config), then forward. Vite forwards through its bundled
// http-proxy; oj prefers the app's http-proxy when it happens to be resolvable
// (giving streaming/ws/changeOrigin/secure/header handling and `configure` for
// free) and otherwise pipes through a faithful node http/https request — which
// preserves streaming bodies (SSE, long-poll, uploads), `changeOrigin` and
// `secure:false`. WebSocket upgrades are NOT handled here: the Rust inbound
// proxy owns them (its listener receives the browser upgrade), and the worker's
// outbound fetch is never a ws upgrade, so nothing regresses.
async function createProxyMiddleware(proxyConfig, appRoot) {
  const contexts = [];
  for (const context of Object.keys(proxyConfig)) {
    let opts = proxyConfig[context];
    if (!opts) continue;
    // Vite: a string is shorthand for { target, changeOrigin: true }.
    if (typeof opts === "string") opts = { target: opts, changeOrigin: true };
    // Vite accepts a string, a URL, or an object target (passing them straight
    // to http-proxy's createProxyServer). Skipping a valid non-string target
    // would re-open the wedge (worker fetch -> catch-all -> hang), so accept any
    // non-null target; the pipe derives host/port/protocol from any of the three.
    if (!opts || opts.target == null) continue;
    // Normalize `rewrite` to a function. Three forms reach here: a FUNCTION
    // (vite-format, called as-is like Vite's `opts.rewrite(req.url)`); a
    // {from,to} OBJECT (oj-format over the JSON bridge — `from` is a regex
    // source, applied as `url.replace(new RegExp(from), to)`, so an anchored
    // `^/api` strips the prefix exactly like the Rust `proxy_target`); and none.
    let rewriteFn = null;
    if (typeof opts.rewrite === "function") {
      rewriteFn = opts.rewrite;
    } else if (opts.rewrite && typeof opts.rewrite === "object" && typeof opts.rewrite.from === "string") {
      const to = typeof opts.rewrite.to === "string" ? opts.rewrite.to : "";
      let re = null;
      try { re = new RegExp(opts.rewrite.from); } catch (e) {
        process.stderr.write(`${OJ} plugin host: server.proxy["${context}"].rewrite.from is not a valid regex: ${(e && e.message) || e}\n`);
      }
      if (re) rewriteFn = (url) => url.replace(re, to);
    }
    contexts.push([context, opts, rewriteFn]);
  }
  if (contexts.length === 0) return null;

  // Vite forwards through `http-proxy-3` (its own dependency). Resolve it the
  // way Vite's own module graph would: from the app's `vite` package first,
  // then the app root, then the plugin host itself. In practice Vite BUNDLES
  // http-proxy-3 into its dist, so it is usually not resolvable as a module and
  // the faithful node pipe below is the real path; when http-proxy IS
  // resolvable, it is the single forward and `configure` is honoured.
  let httpProxyLib = null;
  const requirers = [];
  try { requirers.push(createRequire(createRequire(appRoot + "/package.json").resolve("vite"))); } catch {}
  try { requirers.push(createRequire(appRoot + "/package.json")); } catch {}
  try { requirers.push(createRequire(import.meta.url)); } catch {}
  for (const req of requirers) {
    for (const name of ["http-proxy-3", "http-proxy"]) {
      try {
        const mod = await import(pathToFileURL(req.resolve(name)).href);
        const lib = mod.default ?? mod;
        if (lib && typeof lib.createProxyServer === "function") { httpProxyLib = lib; break; }
      } catch {}
    }
    if (httpProxyLib) break;
  }

  // ONE ProxyServer per entry, built once at setup and reused across requests,
  // with `configure(proxy, opts)` invoked ONCE with the instance and the error
  // handler wired so a proxy error ENDS the response instead of hanging —
  // mirroring Vite's proxyMiddleware setup (dist node.js:19156-19191).
  const proxies = new Map();
  if (httpProxyLib) {
    for (const [context, opts] of contexts) {
      const proxy = httpProxyLib.createProxyServer(opts);
      if (typeof opts.configure === "function") {
        try { opts.configure(proxy, opts); } catch (e) {
          process.stderr.write(`${OJ} plugin host: server.proxy["${context}"].configure threw: ${(e && e.message) || e}\n`);
        }
      }
      // Strip oj's internal signal headers from the target's response so a
      // real upstream emitting them cannot be misread as the host fallthrough.
      proxy.on("proxyRes", (proxyRes) => {
        if (proxyRes && proxyRes.headers) {
          delete proxyRes.headers["x-oj-fallthrough"];
          delete proxyRes.headers["x-oj-rewritten-url"];
        }
      });
      // Vite dist node.js:19167-19187: distinguish a web response (`"req" in
      // res`) — 502 + end when nothing was sent yet — from a ws socket (end).
      proxy.on("error", (err, _req, res) => {
        process.stderr.write(`${OJ} plugin host: http proxy error: ${(err && (err.stack || err.message)) || err}\n`);
        if (res && "req" in res) {
          if (!res.headersSent && !res.writableEnded) {
            try { res.writeHead(502, { "Content-Type": "text/plain" }).end(); } catch {}
          }
        } else if (res && typeof res.end === "function") {
          res.end();
        }
      });
      proxies.set(context, proxy);
    }
    process.stderr.write(`${OJ} plugin host: server.proxy active (${contexts.length} route(s), via http-proxy)\n`);
  } else {
    // The node pipe honours the common options faithfully (target,
    // changeOrigin, secure, rewrite, header + body streaming). Be HONEST about
    // the http-proxy-only options it cannot apply without the proxy instance —
    // warn once per entry instead of silently ignoring them.
    const PIPE_UNSUPPORTED = ["configure", "xfwd", "cookieDomainRewrite", "cookiePathRewrite", "followRedirects", "autoRewrite", "protocolRewrite", "hostRewrite"];
    for (const [context, opts] of contexts) {
      const present = PIPE_UNSUPPORTED.filter((k) => opts[k] != null && opts[k] !== false);
      if (present.length) {
        process.stderr.write(`${OJ} plugin host: server.proxy["${context}"] ${present.join(", ")} need http-proxy (bundled into Vite, not resolvable here); the built-in proxy does not apply them\n`);
      }
    }
    process.stderr.write(`${OJ} plugin host: server.proxy active (${contexts.length} route(s))\n`);
  }

  // Faithful node http/https pipe (used when http-proxy is not resolvable).
  const pipeForward = (opts, req, res) => {
    const target = pipeTarget(opts.target);
    if (!target) {
      res.statusCode = 502;
      res.end(`oj proxy: invalid target ${typeof opts.target === "object" ? JSON.stringify(opts.target) : opts.target}`);
      return;
    }
    const isHttps = target.protocol === "https:";
    const lib = isHttps ? https : http;
    const headers = { ...req.headers };
    // http-proxy's changeOrigin: send the target's host, not the dev server's
    // (otherwise the browser's Host is forwarded unchanged, as http-proxy does).
    if (opts.changeOrigin) headers.host = target.host;
    // Prepend a target base path (http-proxy prependPath) to the (already
    // rewritten) request url.
    const base = target.pathname && target.pathname !== "/" ? target.pathname.replace(/\/$/, "") : "";
    const options = {
      protocol: target.protocol,
      hostname: target.hostname,
      port: target.port || (isHttps ? 443 : 80),
      method: req.method,
      path: base + req.url,
      headers,
    };
    // http-proxy's secure:false accepts self-signed dev certificates.
    if (isHttps && opts.secure === false) options.rejectUnauthorized = false;
    const upstream = lib.request(options, (r) => {
      // Pass the upstream status and ALL headers straight through (Set-Cookie,
      // Location, content-type … untouched), as http-proxy does by default —
      // but STRIP oj's internal signal headers so a real upstream that happens
      // to emit them cannot be misread as the host stack's fallthrough (only
      // the host's runStack may set these).
      const headers = { ...r.headers };
      delete headers["x-oj-fallthrough"];
      delete headers["x-oj-rewritten-url"];
      res.writeHead(r.statusCode || 502, headers);
      r.pipe(res);
    });
    upstream.on("error", (e) => {
      // Mirror http-proxy's error handling: once the response has started the
      // stream is committed, so DESTROY the socket rather than appending an
      // error string onto the partial body (which would corrupt the response).
      // Only synthesize a 502 when nothing has been sent yet.
      if (res.headersSent || res.writableEnded) {
        res.destroy();
      } else {
        // res may already have been destroyed by the client-abort close handler
        // (ERR_STREAM_DESTROYED), so guard the write/end.
        try {
          res.writeHead(502, { "content-type": "text/plain; charset=utf-8" });
          res.end(`oj proxy: ${targetLabel(opts.target)} unreachable: ${e.message}`);
        } catch {}
      }
    });
    // Client disconnect / response error: abort the upstream so its socket is
    // not leaked, and swallow res errors (EPIPE/ECONNRESET) so they do not
    // surface as an unhandled error on the host.
    const abortUpstream = () => { if (!upstream.destroyed) upstream.destroy(); };
    res.on("close", () => { if (!res.writableFinished) abortUpstream(); });
    res.on("error", abortUpstream);
    req.on("error", abortUpstream);
    // Stream the request body through (uploads are never buffered whole).
    req.pipe(upstream);
  };

  const forward = (opts, context, req, res) => {
    const proxy = proxies.get(context);
    if (proxy) proxy.web(req, res, {});
    else pipeForward(opts, req, res);
  };

  return async function ojProxyMiddleware(req, res, next) {
    const url = req.url;
    for (const [context, opts, rewriteFn] of contexts) {
      if (!doesProxyContextMatchUrl(context, url)) continue;
      if (typeof opts.bypass === "function") {
        try {
          const bypassResult = await opts.bypass(req, res, opts);
          if (typeof bypassResult === "string") {
            req.url = bypassResult;
            if (res.writableEnded) return;
            return next();
          }
          if (bypassResult === false) {
            res.statusCode = 404;
            return res.end();
          }
        } catch (e) {
          return next(e);
        }
      }
      // A user rewrite (or the {from,to} regex) is guarded: a throw becomes
      // next(e) (an error the connect stack handles) instead of an
      // unhandledRejection that would hang the request and could tear the host
      // down. Vite calls `opts.rewrite(req.url)` bare; oj wraps it.
      if (rewriteFn) {
        try {
          req.url = rewriteFn(req.url);
        } catch (e) {
          return next(e);
        }
      }
      forward(opts, context, req, res);
      return;
    }
    next();
  };
}

// Match connect routes, ignoring case: "/api" matches "/api", "/api/x"
// and "/api.json", but not "/apiary". Ignore query strings and fragments.
function routeMatches(route, url) {
  const q = url.search(/[?#]/);
  const pathname = q === -1 ? url : url.slice(0, q);
  if (pathname.slice(0, route.length).toLowerCase() !== route.toLowerCase()) return false;
  const c = pathname[route.length];
  return c === undefined || c === "/" || c === ".";
}

async function setupConfigureServer() {
  const stack = [];
  function runStack(req, res, done) {
    let i = 0;
    const next = (err) => {
      while (i < stack.length) {
        const entry = stack[i++];
        const fn = entry.handle;
        if (entry.route && !routeMatches(entry.route, req.url)) continue;
        if ((fn.length >= 4) !== (err != null)) continue;
        try {
          return err != null ? fn(err, req, res, next) : fn(req, res, next);
        } catch (e) {
          return next(e);
        }
      }
      if (typeof done === "function") return done(err);
      if (err != null) {
        res.statusCode = 500;
        res.end(String((err && err.stack) || err));
        return;
      }
      const upstream = req.headers["x-oj-forward-to"];
      if (typeof upstream === "string" && /^\d+$/.test(upstream)) return forwardUpstream(req, res, Number(upstream));
      res.setHeader("x-oj-fallthrough", "1");
      // A proxy `bypass` returning a string rewrote req.url and fell through
      // (nothing in the stack served it): report the rewritten url so the Rust
      // delegate can serve it via normal routing instead of returning this 404.
      try { res.setHeader("x-oj-rewritten-url", req.url); } catch {}
      res.statusCode = 404;
      res.end();
    };
    next();
  }
  // A request oj streams through the chain (a body it cannot replay): when no
  // middleware claims it, pipe it on to the SSR runner the header names, both
  // directions streaming, instead of answering with the fall-through mark.
  function forwardUpstream(req, res, port) {
    if (req.readableDidRead || req.readableEnded) {
      res.statusCode = 500;
      res.end("oj: a configureServer middleware consumed the request body without handling the request");
      return;
    }
    const headers = { ...req.headers };
    delete headers["x-oj-forward-to"];
    // The runner rebuilds Host from x-oj-host (Node sets the loopback Host).
    if (headers.host) headers["x-oj-host"] = headers.host;
    delete headers.host;
    const up = http.request({ host: "127.0.0.1", port, method: req.method, path: req.url, headers }, (r) => {
      res.writeHead(r.statusCode, r.headers);
      r.pipe(res);
    });
    up.on("error", (e) => {
      if (!res.headersSent) res.writeHead(502, { "content-type": "text/plain; charset=utf-8" });
      res.end(`oj: ssr runner unreachable: ${e.message}`);
    });
    req.pipe(up);
  }
  // @cloudflare/vite-plugin uses `server.middlewares` both as a callable
  // connect app (its workerd->node bridge) and via `.use()`, so provide both.
  function middlewares(req, res, done) {
    return runStack(req, res, done);
  }
  // Keep connect's { route, handle } shape so plugins can read handle.name.
  // Trim trailing slashes: use(fn) must store "", not "/", or Nitro would
  // treat it as a route prefix and skip every request.
  middlewares.use = (a, b) => {
    const [route, handle] = typeof a === "function" ? ["/", a] : [a, b];
    stack.push({ route: route.endsWith("/") ? route.slice(0, -1) : route, handle });
  };
  middlewares.stack = stack;
  const noop = () => {};
  const fileWatcher = Object.assign(new EventEmitter(), { add: noop, unwatch: noop, close: noop });
  const server = {
    config: resolvedConfig,
    middlewares,
    httpServer: stubHttpServer(),
    ws: wsApi,
    hot: wsApi,
    watcher: fileWatcher,
    moduleGraph,
    // Vite restarts the dev server (re-reading the config); oj re-execs itself,
    // which is how it already handles a config-file change.
    restart: async () => ojServerEvent("restart"),
    close: async () => {},
    transformIndexHtml: async (_url, html) => transformIndexHtml(html),
    transformRequest: async () => null,
    ssrLoadModule: async () => {
      throw new Error("oj: server.ssrLoadModule is not available in configureServer");
    },
  };
  // A plugin that drives its own dev runtime DECLARES it, Vite's way: its
  // `config` hook returns `environments.<name>.dev.createEnvironment` and
  // resolveConfig merges that into the user config BEFORE default-filling
  // every environment's factory (runConfigHook, then
  // resolveDevEnvironmentOptions). The config extractor ran that declaration
  // mechanism before this host spawned, and its verdict arrives as
  // `initial.runnerBacked`. Extraction TRUE is authoritative and sufficient —
  // a declaring plugin whose config hook failed IN THIS HOST keeps the path:
  // the extractor's independent run already decided, and buildEnvironments
  // re-resolves on fresh instances where the hook may well succeed. Extraction
  // FALSE or ABSENT is NOT final: it falls through to the host's own
  // declaration check (the raw user config, oj's initial config, and
  // pluginConfigDelta — never the Vite-RESOLVED config, whose every
  // environment carries a default factory), so a missing or stale-false
  // verdict (an extraction path that degraded, a cache that outlived a
  // wrangler fix) can never silently disable the worker path when the host
  // itself can see the declaration.
  const declaresRunnerEnvironment = (cfg) =>
    !!cfg &&
    typeof cfg === "object" &&
    Object.values(cfg.environments ?? {}).some(
      (e) => e && e.dev && typeof e.dev.createEnvironment === "function",
    );
  const extractionSaysBacked = initial.runnerBacked === true;
  const hostSeesDeclaration =
    !extractionSaysBacked &&
    [userViteConfig, initial.config, pluginConfigDelta].some(declaresRunnerEnvironment);
  const runnerDeclared = extractionSaysBacked || hostSeesDeclaration;
  const runnerBackedSource = extractionSaysBacked
    ? "config extraction"
    : initial.runnerBacked === false
      ? "host declaration check (extraction said no)"
      : "host declaration check";
  if (runnerDeclared) {
    process.stderr.write(
      `${OJ} plugin host: a custom dev environment is declared (decided by ${runnerBackedSource})\n`,
    );
    try {
      const built = await buildEnvironments(server);
      // Every createEnvironment/init can fail individually (workerd refused to
      // boot, a port clash): an empty result must not count as runner-backed,
      // or Rust would idle its SSR runner with nothing serving documents. The
      // stub fallback below then still applies.
      if (built && Object.keys(built).length > 0) {
        server.environments = built;
        runnerEnvironmentsBuilt = true;
      }
    } catch (e) {
      process.stderr.write(`${OJ} plugin host: buildEnvironments failed: ${(e && e.message) || e}\n`);
    }
  }
  // Hoisted out of the gate block: whenever detection said true and no
  // environment came up — whatever the reason (buildEnvironments threw, every
  // factory failed, the app's Vite would not load) — say so instead of
  // degrading to the Node SSR runner silently (the per-failure causes are on
  // stderr above).
  if (runnerDeclared && !runnerEnvironmentsBuilt) {
    process.stderr.write(
      `${OJ} warning: a plugin declared a custom dev environment but none came up; documents are served by the Node SSR runner\n`,
    );
  }
  // Vite's createServer always exposes `server.environments` with at least
  // client and ssr (a DevEnvironment per config.environments entry, defaults
  // included). Real DevEnvironments from the app's Vite are only built for
  // plugins that drive runners (above, and that set is then the app's Vite's
  // to define); otherwise every environment is an oj-backed stand-in so
  // configureServer code reading server.environments.client / .ssr does not throw.
  if (!server.environments) {
    server.environments = {};
    const envNames = new Set(["client", "ssr", ...Object.keys(resolvedConfig?.environments ?? {})]);
    for (const name of envNames) server.environments[name] = stubEnvironment(name, server);
  }
  devServer = server;
  const post = [];
  for (const { p, fn } of pluginsWithHook("configureServer")) {
    let r;
    initStage(`configureServer:${p.name ?? "?"}`);
    try {
      r = await fn.call(ctxFor(p), server);
    } catch (e) {
      if (!ojStartMode) throw e;
      process.stderr.write(`${OJ} plugin host: configureServer(${p.name ?? "?"}) skipped: ${(e && e.message) || e}\n`);
      continue;
    }
    if (typeof r === "function") post.push(r);
  }
  // Register `server.proxy` here — after the configureServer PRE hooks, before
  // the POST hooks — exactly as Vite orders it (dist: proxyMiddleware is added
  // right before postHooks run). This places the single proxy BEFORE a plugin's
  // catch-all (the Cloudflare worker dispatch, registered in a returned POST
  // hook), so a matched request (browser-delegated or the worker's own outbound
  // fetch) is proxied instead of falling through into the worker. A vite-format
  // app's proxy is read straight from the loaded vite config, so a FUNCTION
  // rewrite / configure / bypass survive; an oj-config-format app has no
  // loadable vite config, so its `server.proxy` (with a {from,to} rewrite that
  // DOES cross the JSON bridge) arrives in the spawn payload's `initial.config`.
  const proxyConfig =
    userViteConfig?.server?.proxy ?? initial.config?.server?.proxy ?? resolvedConfig?.server?.proxy;
  if (proxyConfig && typeof proxyConfig === "object") {
    try {
      const proxyMw = await createProxyMiddleware(proxyConfig, initial.config?.root ?? process.cwd());
      if (proxyMw) middlewares.use(proxyMw);
    } catch (e) {
      process.stderr.write(`${OJ} plugin host: failed to set up server.proxy: ${(e && e.stack) || e}\n`);
    }
  }
  for (const fn of post) {
    try {
      await fn();
    } catch (e) {
      if (!ojStartMode) throw e;
      process.stderr.write(`${OJ} plugin host: post configureServer skipped: ${(e && e.message) || e}\n`);
    }
  }
  // Follow Vite's startup order: client buildStart, then environment listen().
  // buildStart sets up shared state, including the CSS modules cache used by
  // server transforms. listen() enables runner messages and warmup.
  // Skip client listen(): oj serves the client, so Vite need not scan its deps.
  if (runnerEnvironmentsBuilt) {
    try {
      await server.environments.client?.pluginContainer?.buildStart?.();
    } catch (e) {
      process.stderr.write(`${OJ} plugin host: client buildStart failed: ${(e && e.message) || e}\n`);
    }
    await Promise.all(Object.entries(server.environments).map(async ([name, e]) => {
      if (name === "client" || typeof e?.listen !== "function") return;
      try {
        await e.listen(server);
      } catch (err) {
        process.stderr.write(`${OJ} plugin host: env.listen(${name}) failed: ${(err && err.message) || err}\n`);
      }
    }));
  }
  // oj listens as soon as the host is ready (before any hook request arrives);
  // `httpServer.once("listening")` handlers registered in configureServer fire
  // here like they would under Vite's listen().
  server.httpServer.emit("listening");
  if (stack.length === 0) return;

  const srv = http.createServer((req, res) => {
    // The browser's Host travels as x-oj-host (hyper owns the loopback Host):
    // middlewares read req.headers.host as they do under Vite.
    if (typeof req.headers["x-oj-host"] === "string") {
      req.headers.host = req.headers["x-oj-host"].split(",")[0].trim() || req.headers.host;
      delete req.headers["x-oj-host"];
    }
    if (req.method === "POST" && req.url === "/__oj_invalidate") {
      let body = "";
      req.on("data", (c) => (body += c));
      req.on("end", async () => {
        let changes = [];
        let resync = false;
        try {
          const parsed = JSON.parse(body || "{}");
          resync = parsed.resync === true;
          changes = parsed.changes
            || (parsed.paths || []).map((p) => ({ path: p, type: "update" }));
        } catch {}
        if (resync) {
          // ACK on ENQUEUE (202-style): the queue is serialized, so an
          // enqueued resync is guaranteed to run after everything already
          // queued — answering only after the (possibly long) drain made the
          // caller's bounded client time out and retry a resync that was
          // already coming. The ack therefore only means "enqueued"; when the
          // resync EXECUTES, a control-plane `{ ojResyncDone }` push tells
          // the Rust side, which claims "resynced" only then (a stuck queue
          // warns instead of logging success). Idempotent and coalesced: a
          // pending resync invalidates whole graphs, so it already covers
          // everything a duplicate would — duplicates (a caller retry racing
          // a slow queue) are absorbed instead of stacking full-reloads, and
          // one done push may answer several coalesced enqueues.
          if (!resyncPending) {
            resyncPending = true;
            invalidateQueue = invalidateQueue
              .then(() => {
                resyncPending = false;
                resyncEnvironments(server.environments);
                ctl({ ojResyncDone: true });
              })
              .catch(() => {
                resyncPending = false;
              });
          }
          res.statusCode = 204;
          res.end();
          return;
        }
        // Answer only once the invalidation is done, so the Rust side's POST
        // completing means a next request cannot be served from stale modules.
        // Serialized: the early (pre-settle) and settled sends for one edit
        // batch must not interleave their module-graph walks.
        invalidateQueue = invalidateQueue
          .then(() => invalidateEnvironments(server.environments, fileWatcher, changes))
          .catch(() => {});
        await invalidateQueue;
        res.statusCode = 204;
        res.end();
      });
      return;
    }
    middlewares(req, res);
  });
  await new Promise((resolve) => srv.listen(0, "127.0.0.1", resolve));
  middlewarePort = srv.address().port;
  process.stderr.write(`${OJ} plugin host: configureServer middleware on :${middlewarePort}\n`);
}
// Re-pushed until Rust ACKs ({ ojServeInfoAck } on stdin): the serve info is a
// one-shot, state-bearing push, and a copy spliced by a plugin's partial stdout
// write must not be lost forever. Bounded so a driver that never ACKs (tests
// poking the host directly) is not spammed indefinitely.
let serveInfoRepush = null;
function stopServeInfoRepush() {
  if (serveInfoRepush) clearInterval(serveInfoRepush);
  serveInfoRepush = null;
}
if (env.command !== "build") {
  await setupConfigureServer();
  // Push the serve info the moment it exists (like the {ojWs} pushes): the RPC
  // listener below only registers after every top-level await, so on a slow
  // boot (many plugins, Miniflare) Rust's boot-time RPCs cannot be answered and
  // the worker path would silently never activate. The push reaches Rust
  // whenever the host comes up, however late, and Rust flips to the middleware
  // then (it also flips Rust's "initialized" gate for RPC timeouts).
  const pushServeInfo = () =>
    ctl({ ojServeInfo: { middlewarePort, runnerEnvironments: runnerEnvironmentsBuilt } });
  pushServeInfo();
  let repushes = 0;
  serveInfoRepush = setInterval(() => {
    if (++repushes > 120) return stopServeInfoRepush();
    pushServeInfo();
  }, 1000);
}

if (ssrBridgeDir) {
  (async () => {
    try {
      const bridgePath = join(dirname(fileURLToPath(import.meta.url)), "start", "vite-plugin-bridge.mjs");
      const bridge = await import(pathToFileURL(bridgePath).href);
      const c = bridge.createPluginContainer({ parseAst: viteParseAst }, allPlugins, {
        command: env.command,
        mode: env.mode,
        environment: "ssr",
      });
      await c.buildStart();
      ssrContainer = c;
      process.stderr.write(`${OJ} plugin host: ssr container ready (${c.pluginCount} plugin(s), shared config eval)\n`);
    } catch (e) {
      process.stderr.write(`${OJ} plugin host: ssr container failed: ${(e && (e.stack || e.message)) || e}\n`);
    }
    for (const [k, v] of Object.entries(process.env)) {
      if (ssrEnvBase[k] !== v) ssrEnvDelta[k] = v;
    }
    try { writeFileSync(join(ssrBridgeDir, "ready"), "1"); } catch {}
    if (process.env.OJ_BOOT_PHASES) {
      process.stderr.write(`[oj-phase] ${Date.now()} container: bootstrap done\n`);
    }
    ssrResolveReady();
  })();
}

async function transform(code, id, resolvedJson) {
  const bucket = new Set();
  const chunkBucket = [];
  let resolveMap = null;
  if (resolvedJson) {
    try {
      resolveMap = new Map(Object.entries(JSON.parse(resolvedJson)));
    } catch {}
  }
  const runLoop = async () => {
    if (id) seenIds.add(id);
    let current = code;
    const maps = [];
    const transformOptions = { ssr: environment && environment.name === "ssr" };
    // Vite sorts a hook's plugins by the hook's own `order` (pre, normal, post)
    // on top of the plugin's enforce; getSortedPluginsByHook applies to transform
    // like to every other hook.
    for (const { p, fn: handler } of pluginsWithHook("transform")) {
      if (!hookTransformMatches(p.transform, id, current)) continue;
      let r;
      try {
        r = await handler.call(ctxFor(p), current, id, transformOptions);
      } catch (e) {
        throw decoratePluginError(e, p, id);
      }
      if (r == null) continue;
      if (typeof r === "string") {
        current = r;
        continue;
      }
      if (r.code != null) current = r.code;
      if (r.map != null) maps.push(typeof r.map === "string" ? r.map : JSON.stringify(r.map));
      updateModuleInfo(id, r);
    }
    const prior = moduleInfoCache.get(id);
    const info = makeModuleInfo(id, current, prior ? { meta: prior.meta, moduleSideEffects: prior.moduleSideEffects, syntheticNamedExports: prior.syntheticNamedExports } : null);
    moduleInfoCache.set(id, info);
    seenIds.add(id);
    for (const { p, fn } of pluginsWithHook("moduleParsed")) await fn.call(ctxFor(p), info);
    return JSON.stringify({
      code: current,
      watchFiles: [...bucket],
      maps,
      emittedChunks: chunkBucket,
    });
  };
  return transformWatchStore.run(bucket, () =>
    transformEmitStore.run(chunkBucket, () =>
      transformResolveStore.run(resolveMap, runLoop),
    ),
  );
}

async function replayModuleParsed(id) {
  if (id) seenIds.add(id);
  const info = moduleInfoCache.get(id) ?? makeModuleInfo(id, "");
  moduleInfoCache.set(id, info);
  for (const { fn } of pluginsWithHook("moduleParsed")) await fn.call(ctx, info);
  return null;
}

const anyModuleParsed = () => pluginsWithHook("moduleParsed").length > 0;

async function watchChange(id, event) {
  for (const { fn } of pluginsWithHook("watchChange")) await fn.call(ctx, id, { event });
  return null;
}

// Any hook can be a bare function or Vite's object form { order, filter, handler }
// (createVirtualModule, import-protection, and others use it). Vite unwraps every
// hook through getHookHandler and sorts the plugins of a hook by its `order`
// (pre / normal / post, stable within a rank); do the same for every hook here.
function hookHandler(hook) {
  if (typeof hook === "function") return hook;
  if (hook && typeof hook.handler === "function") return hook.handler;
  return null;
}
function hookFn(plugin, name) {
  return plugin ? hookHandler(plugin[name]) : null;
}
function hookOrderRank(hook) {
  const o = hook && typeof hook === "object" ? hook.order : undefined;
  return o === "pre" ? -1 : o === "post" ? 1 : 0;
}
function pluginsWithHook(name) {
  const out = [];
  for (const p of plugins) {
    const fn = hookFn(p, name);
    if (fn) out.push({ p, fn, rank: hookOrderRank(p[name]) });
  }
  out.sort((a, b) => a.rank - b.rank);
  return out;
}

// Vite's pluginFilter: a string *id* filter is a picomatch glob (dot:true) joined
// to cwd unless it starts with `**` or is absolute, a RegExp is tested against the
// slash-normalized id (lastIndex reset), and a string *code* filter is a substring.
// The glob dialect covered is what plugins use: `*`, `**`, `?`, `{a,b}`, `[...]`.
const slash = (p) => p.replace(/\\/g, "/");
function globToRegExpSource(glob) {
  let re = "";
  for (let i = 0; i < glob.length; i++) {
    const c = glob[i];
    if (c === "*") {
      if (glob[i + 1] === "*") {
        i++;
        if (glob[i + 1] === "/") {
          i++;
          re += "(?:.*/)?";
        } else {
          re += ".*";
        }
      } else {
        re += "[^/]*";
      }
    } else if (c === "?") {
      re += "[^/]";
    } else if (c === "{") {
      const j = glob.indexOf("}", i);
      if (j > i) {
        re += "(?:" + glob.slice(i + 1, j).split(",").map(globToRegExpSource).join("|") + ")";
        i = j;
      } else {
        re += "\\{";
      }
    } else if (c === "[") {
      const j = glob.indexOf("]", i);
      if (j > i) {
        let cls = glob.slice(i + 1, j);
        if (cls[0] === "!") cls = "^" + cls.slice(1);
        re += "[" + cls + "]";
        i = j;
      } else {
        re += "\\[";
      }
    } else if (c === "\\" && i + 1 < glob.length) {
      re += "\\" + glob[++i];
    } else {
      re += c.replace(/[.+^$()|]/g, "\\$&");
    }
  }
  return re;
}
const idGlobCache = new Map();
function idPatternToRegExp(pattern) {
  if (pattern instanceof RegExp) return pattern;
  let re = idGlobCache.get(pattern);
  if (!re) {
    const cwd = initial.config?.root ?? process.cwd();
    const glob = pattern.startsWith("**") || isAbsolute(pattern) ? slash(pattern) : slash(join(cwd, pattern));
    re = new RegExp("^" + globToRegExpSource(glob) + "$");
    idGlobCache.set(pattern, re);
  }
  return re;
}
function matchId(pattern, id) {
  if (!(pattern instanceof RegExp) && typeof pattern !== "string") return false;
  const re = idPatternToRegExp(pattern);
  const r = re.test(slash(id));
  re.lastIndex = 0;
  return r;
}
function matchCode(pattern, code) {
  if (pattern instanceof RegExp) {
    const r = pattern.test(code);
    pattern.lastIndex = 0;
    return r;
  }
  return typeof pattern === "string" && code.includes(pattern);
}
function matchList(patterns, value, matcher) {
  if (patterns == null) return false;
  return (Array.isArray(patterns) ? patterns : [patterns]).some((p) => matcher(p, value));
}
function filterMatches(spec, value, matcher) {
  if (spec == null) return true;
  if (spec instanceof RegExp || typeof spec === "string" || Array.isArray(spec)) {
    return matchList(spec, value, matcher);
  }
  if (spec.exclude != null && matchList(spec.exclude, value, matcher)) return false;
  if (spec.include != null) return matchList(spec.include, value, matcher);
  return true;
}
// Rolldown's moduleType for an id, from its extension (what `filter.moduleType`
// compares against); anything unknown is "js".
function moduleTypeOf(id) {
  const clean = String(id).split("?", 1)[0].split("#", 1)[0];
  const m = /\.([a-z0-9]+)$/i.exec(clean);
  const ext = m ? m[1].toLowerCase() : "";
  if (["js", "jsx", "ts", "tsx", "json", "css", "text", "base64", "dataurl", "binary", "empty", "asset"].includes(ext)) {
    return ext;
  }
  if (ext === "mjs" || ext === "cjs") return "js";
  if (ext === "mts" || ext === "cts") return "ts";
  return "js";
}

function hookIdMatches(hook, id) {
  const f = hook && typeof hook === "object" ? hook.filter : null;
  return !f || filterMatches(f.id, id, matchId);
}

function hookTransformMatches(hook, id, code) {
  const f = hook && typeof hook === "object" ? hook.filter : null;
  if (!f) return true;
  if (!filterMatches(f.id, id, matchId)) return false;
  if (f.code != null && !filterMatches(f.code, code, matchCode)) return false;
  if (f.moduleType != null && !filterMatches(f.moduleType, moduleTypeOf(id), (p, v) => p === v)) return false;
  return true;
}

// The resolveId chain as Vite's pluginContainer runs it: plugins in hook order,
// each handed `{ attributes, custom, isEntry, ssr, scan }`, the first non-null
// result winning with its object kept whole (id plus external / meta /
// moduleSideEffects / syntheticNamedExports, as Vite's partial does).
// `opts.skipCalls`: the chain's cumulative skip list (see ctx.resolve);
// `opts.skip`: a single plugin to skip outright (legacy single-caller form,
// kept for external callers of environment.resolveId).
async function resolveIdFull(source, importer, opts) {
  const options = {
    attributes: (opts && opts.attributes) || {},
    custom: (opts && opts.custom) || {},
    isEntry: !!(opts && opts.isEntry),
    ssr: environment.config?.consumer === "server",
    scan: false,
  };
  const skip = opts && opts.skip;
  const skipCalls = (opts && opts.skipCalls) || null;
  for (const { p, fn: handler } of pluginsWithHook("resolveId")) {
    if (skip && p === skip) continue;
    // Vite merges skipCalls into the skip set: a plugin is skipped when its
    // recorded call re-issues the same id + importer, or when it already
    // re-entered once (`called`) -- the recursion hard-stop.
    if (skipCalls && skipCalls.some((c) => c.plugin === p && (c.called || (c.id === source && c.importer === importer)))) continue;
    if (!hookIdMatches(p.resolveId, source)) continue;
    let r;
    try {
      r = await handler.call(resolveHookCtx(p, skipCalls), source, importer || undefined, options);
    } catch (e) {
      throw decoratePluginError(e, p, importer || source);
    }
    if (r == null) continue;
    if (typeof r === "string") return { id: r };
    if (typeof r !== "object" || r.id == null) continue;
    const out = { id: r.id };
    for (const k of ["external", "meta", "moduleSideEffects", "syntheticNamedExports", "attributes"]) {
      if (r[k] !== undefined) out[k] = r[k];
    }
    return out;
  }
  return null;
}
async function resolveId(source, importer, opts) {
  const r = await resolveIdFull(source, importer, opts);
  return r == null ? null : r.id;
}

// The load chain (Vite pluginContainer.load): `{ ssr }` options, the first
// non-null result returned whole (code, map, meta) and its meta folded into
// the module info.
async function loadFull(id) {
  const options = { ssr: environment.config?.consumer === "server" };
  for (const { p, fn: handler } of pluginsWithHook("load")) {
    if (!hookIdMatches(p.load, id)) continue;
    let r;
    try {
      r = await handler.call(ctxFor(p), id, options);
    } catch (e) {
      throw decoratePluginError(e, p, id);
    }
    if (r == null) continue;
    if (typeof r === "string") return { code: r };
    if (typeof r !== "object" || r.code == null) continue;
    updateModuleInfo(id, r);
    return r;
  }
  return null;
}
async function load(id) {
  const r = await loadFull(id);
  return r == null ? null : r.code;
}

async function readModifiedFile(file) {
  const content = await readFile(file, "utf8");
  if (content) return content;
  const mtime = (await fsStat(file)).mtimeMs;
  for (let n = 0; n < 10; n++) {
    await new Promise((r) => setTimeout(r, 10));
    const newMtime = (await fsStat(file)).mtimeMs;
    if (newMtime !== mtime) break;
  }
  return readFile(file, "utf8");
}

async function handleHotUpdate(file, timestamp, type, modulesJson) {
  let modules;
  try {
    modules = modulesJson ? JSON.parse(modulesJson) : [];
  } catch {
    modules = [];
  }
  // Vite's HmrContext / HotUpdateOptions both carry `server`; the ubiquitous
  // `handleHotUpdate({ file, server }) { server.ws.send(...) }` idiom needs it.
  const hmrContext = {
    file,
    timestamp: Number(timestamp),
    type: type || "update",
    modules,
    read: () => readModifiedFile(file),
    server: devServer,
  };
  let filtered = null;
  for (const { p, fn } of hotUpdatePlugins()) {
    let r;
    try {
      r = await fn.call(ctxFor(p), hmrContext);
    } catch (e) {
      // Vite records the failing hook's error and sends it to the client
      // (hmr.ts); name the plugin so the overlay says who threw.
      throw decoratePluginError(e, p, file);
    }
    if (r === "full-reload") return "full-reload";
    if (Array.isArray(r)) {
      hmrContext.modules = r;
      filtered = r
        .map((m) => (typeof m === "string" ? m : m && (m.url ?? m.id)))
        .filter((u) => typeof u === "string" && u.length > 0);
    }
  }
  if (filtered === null) return null;
  if (filtered.length === 0) return "skip";
  return JSON.stringify({ action: "filter", modules: filtered });
}

// Vite 6+: `plugin.hotUpdate ?? plugin.handleHotUpdate` (the Environment API hook
// takes precedence, with the same context shape plus this.environment).
function hotUpdatePlugins() {
  const out = [];
  for (const p of plugins) {
    const hook = p.hotUpdate != null ? p.hotUpdate : p.handleHotUpdate;
    const fn = hookHandler(hook);
    if (fn) out.push({ p, fn, rank: hookOrderRank(hook) });
  }
  out.sort((a, b) => a.rank - b.rank);
  return out;
}

function escapeAttr(v) {
  return String(v).replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/</g, "&lt;");
}
function renderTag(t) {
  const attrs = Object.entries(t.attrs ?? {})
    .map(([k, v]) => (v === true ? ` ${k}` : v === false || v == null ? "" : ` ${k}="${escapeAttr(v)}"`))
    .join("");
  const inner = t.children ?? "";
  const voidTag = ["meta", "link", "base"].includes(t.tag);
  return voidTag ? `<${t.tag}${attrs}>` : `<${t.tag}${attrs}>${inner}</${t.tag}>`;
}

function injectTags(html, tags) {
  const at = { "head-prepend": [], head: [], "body-prepend": [], body: [] };
  // Vite's default injection point is head-prepend, not head-append.
  for (const t of tags) (at[t.injectTo ?? "head-prepend"] ?? at["head-prepend"]).push(renderTag(t));
  const put = (h, marker, html2, after) => {
    const i = h.indexOf(marker);
    if (i === -1) return h + html2;
    const at2 = after ? i + marker.length : i;
    return h.slice(0, at2) + html2 + h.slice(at2);
  };
  html = put(html, "<head>", at["head-prepend"].join(""), true);
  html = put(html, "</head>", at.head.join(""), false);
  html = put(html, "<body>", at["body-prepend"].join(""), true);
  html = put(html, "</body>", at.body.join(""), false);
  return html;
}

function htmlHookRank(hook) {
  const order = hook && typeof hook === "object" ? hook.order ?? hook.enforce : undefined;
  return order === "pre" ? -1 : order === "post" ? 1 : 0;
}
// `ctxJson`: Vite's IndexHtmlTransformContext for this page. Dev (indexHtml
// middleware): `{ path: url, filename: <abs file>, originalUrl }` plus the dev
// server; build (html.ts): `{ path: "/" + rel, filename: <abs file>, bundle,
// chunk }`. A throwing hook fails the request / build under Vite (the error
// reaches the error middleware), so it is rethrown decorated, not swallowed.
async function transformIndexHtml(html, ctxJson) {
  let current = html;
  let htmlCtx = null;
  try {
    htmlCtx = ctxJson ? JSON.parse(ctxJson) : null;
  } catch {}
  if (!htmlCtx || typeof htmlCtx !== "object") htmlCtx = {};
  if (htmlCtx.path == null) htmlCtx.path = "/index.html";
  if (htmlCtx.filename == null) htmlCtx.filename = pathResolve(resolvedConfig?.root ?? initial.config?.root ?? process.cwd(), htmlCtx.path.replace(/^\//, ""));
  if (env.command !== "build" && devServer) htmlCtx.server = devServer;
  // Honor per-hook order: 'pre' hooks run first, 'post' last (stable within a rank).
  const entries = [];
  for (const p of plugins) {
    const hook = p.transformIndexHtml;
    const fn = typeof hook === "function" ? hook : hook?.handler ?? hook?.transform;
    if (typeof fn !== "function") continue;
    entries.push({ p, fn, rank: htmlHookRank(hook) });
  }
  entries.sort((a, b) => a.rank - b.rank);
  for (const { p, fn } of entries) {
    let r;
    try {
      r = await fn.call(ctxFor(p), current, htmlCtx);
    } catch (e) {
      throw decoratePluginError(e, p, htmlCtx.filename);
    }
    if (r == null) continue;
    if (typeof r === "string") current = r;
    else if (Array.isArray(r)) current = injectTags(current, r);
    else {
      if (typeof r.html === "string") current = r.html;
      if (Array.isArray(r.tags)) current = injectTags(current, r.tags);
    }
  }
  return current;
}

// A throwing lifecycle hook (buildStart, buildEnd, renderStart, generateBundle,
// writeBundle, closeBundle) rejects the whole build in rolldown/Vite; surface it
// as `[plugin:name] message` and let the caller fail instead of logging on.
async function runHookOrThrow(p, fn, args) {
  try {
    return await fn.apply(ctxFor(p), args);
  } catch (e) {
    throw decoratePluginError(e, p);
  }
}

// `buildEnd(error?)`: Rollup hands the hook the error that failed the build
// (Vite's pluginContainer.close and rolldown both do), so a failed build still
// reaches every plugin's cleanup with the cause.
let closeBundleDone = false;
async function runLifecycle(hook, args) {
  // Rollup runs closeBundle once per build; oj reaches it from rolldown's own
  // failure path and from its explicit call, so the second is a no-op.
  if (hook === "closeBundle") {
    if (closeBundleDone) return null;
    closeBundleDone = true;
  }
  let hookArgs = [];
  if (hook === "buildEnd" && args && args[0]) {
    const err = new Error(String(args[0]));
    err.code = "BUILD_FAILED";
    hookArgs = [err];
  }
  for (const { p, fn } of pluginsWithHook(hook)) await runHookOrThrow(p, fn, hookArgs);
  return null;
}

async function runBuildStart() {
  const chunkBucket = [];
  // Rollup passes NormalizedInputOptions; plugins such as @crxjs read
  // `options.input` to decide whether to emit their root chunk, so it must be
  // present. Chunks emitted here are drained to Rust as rolldown build roots.
  const options = {
    input: resolvedConfig?.build?.rollupOptions?.input ?? "index.html",
  };
  await transformEmitStore.run(chunkBucket, async () => {
    for (const { p, fn } of pluginsWithHook("buildStart")) await runHookOrThrow(p, fn, [options]);
  });
  return JSON.stringify({ emittedChunks: chunkBucket });
}

async function generateBundle(bundleJson, isWrite) {
  const bundle = JSON.parse(bundleJson || "{}");
  const outputOptions = environment.config?.build ?? {};
  for (const { p, fn } of pluginsWithHook("generateBundle")) {
    await runHookOrThrow(p, fn, [outputOptions, bundle, isWrite]);
  }
  return JSON.stringify(bundle);
}

async function renderChunk(code, chunkJson) {
  const chunk = JSON.parse(chunkJson || "{}");
  // Rollup passes NormalizedOutputOptions as the 3rd arg; renderChunk hooks such
  // as UnoCSS's read `options.dir` (the resolved output dir) to key their
  // vite:css-post lookup, so provide it resolved against the root.
  const outDir = resolvedConfig?.build?.outDir ?? "dist";
  const root = resolvedConfig?.root ?? initial.config?.root ?? process.cwd();
  const options = { dir: pathResolve(root, outDir), format: "es" };
  let current = code;
  for (const { p, fn } of pluginsWithHook("renderChunk")) {
    const r = await fn.call(ctxFor(p), current, chunk, options);
    if (r == null) continue;
    current = typeof r === "string" ? r : (r.code ?? current);
  }
  return current;
}

async function writeBundle(bundleJson, isWrite) {
  const bundle = JSON.parse(bundleJson || "{}");
  const outputOptions = environment.config?.build ?? {};
  for (const { p, fn } of pluginsWithHook("writeBundle")) {
    await runHookOrThrow(p, fn, [outputOptions, bundle, isWrite]);
  }
  return null;
}

async function run(hook, args) {
  if (hook === "seedChunkNames") {
    try {
      const m = JSON.parse(args[0] ?? "{}");
      for (const [k, v] of Object.entries(m)) chunkFileNames.set(k, v);
    } catch {}
    return null;
  }
  if (hook === "transform") return transform(args[0], args[1], args[2]);
  if (hook === "resolveId") return resolveId(args[0], args[1]);
  if (hook === "load") return load(args[0]);
  if (hook === "handleHotUpdate") return handleHotUpdate(args[0], args[1], args[2], args[3]);
  if (hook === "transformIndexHtml") return transformIndexHtml(args[0], args[1]);
  if (hook === "buildStart") return runBuildStart();
  if (hook === "buildEnd" || hook === "renderStart" || hook === "closeBundle") {
    return runLifecycle(hook, args);
  }
  if (hook === "wsConnection") return wsConnection();
  if (hook === "watchChange") return watchChange(args[0], args[1]);
  if (hook === "getEmittedFiles") {
    return JSON.stringify(emitted.map(({ fileName, source }) => ({ fileName, source })));
  }
  if (hook === "getPluginCss") return JSON.stringify(ojPluginCss);
  if (hook === "getWatchFiles") return JSON.stringify([...watchedFiles]);
  if (hook === "hasModuleParsed") return String(anyModuleParsed());
  if (hook === "replayModuleParsed") return replayModuleParsed(args[0]);
  if (hook === "hasGenerateBundle") {
    return String(pluginsWithHook("generateBundle").length > 0);
  }
  if (hook === "generateBundle") return generateBundle(args[0], args[1] === "true");
  if (hook === "hasRenderChunk") {
    return String(pluginsWithHook("renderChunk").length > 0);
  }
  if (hook === "renderChunk") return renderChunk(args[0], args[1]);
  if (hook === "hasWriteBundle") {
    return String(pluginsWithHook("writeBundle").length > 0);
  }
  if (hook === "writeBundle") return writeBundle(args[0], args[1] === "true");
  if (hook === "getPluginCount") return String(plugins.length);
  if (hook === "getPluginConfig") {
    // JSON-safe subset of what config() hooks returned (functions/RegExps drop).
    let define = null;
    try {
      define = pluginConfigDelta.define ? JSON.parse(JSON.stringify(pluginConfigDelta.define)) : null;
    } catch {}
    return JSON.stringify({ define });
  }
  if (hook === "getEnvDelta") {
    // config() hooks ran at module init (top-level await), so the diff vs the
    // process-start snapshot is final by the time any stdio hook is answered.
    const delta = {};
    for (const [k, v] of Object.entries(process.env)) {
      if (ssrEnvBase[k] !== v) delta[k] = v;
    }
    return JSON.stringify(delta);
  }
  if (hook === "getDepTransformFilters") {
    // A transform's own `filter.code` include patterns. A dep is only worth
    // transforming when its source matches one of these, so oj gates dep
    // transforms on them (like Vite runs a transform only where its filter
    // matches). Transforms with no code filter are app-scoped here (they no-op
    // on deps), which keeps oj from an RPC per dependency module.
    const pats = [];
    for (const p of plugins) {
      const t = p && p.transform;
      const f = t && typeof t === "object" ? t.filter : null;
      let inc = f && f.code;
      if (inc && typeof inc === "object" && !(inc instanceof RegExp) && !Array.isArray(inc)) {
        inc = inc.include;
      }
      for (const r of Array.isArray(inc) ? inc : inc != null ? [inc] : []) {
        if (r instanceof RegExp) pats.push(r.source);
        else if (typeof r === "string") pats.push(r.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
      }
    }
    return JSON.stringify(pats);
  }
  if (hook === "getHasTransform") {
    return String(pluginsWithHook("transform").length > 0);
  }
  if (hook === "getDepLoadFilters") {
    // Object-form `load` hooks' `filter.id` include patterns: a node_modules
    // module is offered to plugin load only when one matches its path.
    const pats = [];
    for (const p of plugins) {
      const l = p && p.load;
      const f = l && typeof l === "object" ? l.filter : null;
      let inc = f && f.id;
      if (inc && typeof inc === "object" && !(inc instanceof RegExp) && !Array.isArray(inc)) {
        inc = inc.include;
      }
      for (const r of Array.isArray(inc) ? inc : inc != null ? [inc] : []) {
        if (r instanceof RegExp) pats.push(r.source);
        else if (typeof r === "string") pats.push(r.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
      }
    }
    return JSON.stringify(pats);
  }
  if (hook === "getResolveIdFilters") {
    // Object-form `resolveId` hooks' `filter.id` include patterns. Vite offers
    // every import (relative and absolute too) to plugin resolveId before its
    // own resolver; oj resolves natively first and routes a non-bare import
    // through the plugins only when one of these patterns claims it.
    const pats = [];
    for (const p of plugins) {
      const r = p && p.resolveId;
      const f = r && typeof r === "object" ? r.filter : null;
      let inc = f && f.id;
      if (inc && typeof inc === "object" && !(inc instanceof RegExp) && !Array.isArray(inc)) {
        inc = inc.include;
      }
      for (const x of Array.isArray(inc) ? inc : inc != null ? [inc] : []) {
        if (x instanceof RegExp) pats.push(x.source);
        else if (typeof x === "string") pats.push(x.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
      }
    }
    return JSON.stringify(pats);
  }
  if (hook === "getHasLoad") {
    const has = plugins.some((p) => typeof hookHandler(p.load) === "function");
    return String(has);
  }
  if (hook === "getHmrHooks") {
    const watchChangeHook = pluginsWithHook("watchChange").length > 0;
    const hotUpdateHook = hotUpdatePlugins().length > 0;
    return JSON.stringify({ watchChange: watchChangeHook, handleHotUpdate: hotUpdateHook });
  }
  if (hook === "getServeInfo") {
    return JSON.stringify({ middlewarePort, runnerEnvironments: runnerEnvironmentsBuilt });
  }
  if (hook === "wsMessage") {
    const event = args[0];
    const data = args[1] ? JSON.parse(args[1]) : null;
    const a = wsListeners.get(event);
    if (a) {
      const client = { send: (e, d) => wsApi.send(e, d) };
      for (const cb of [...a]) {
        try {
          cb(data, client);
        } catch (e) {
          process.stderr.write(`${OJ} ws.on(${event}) handler failed: ${(e && e.stack) || e}\n`);
        }
      }
    }
    return null;
  }
  return null;
}

const hardExit = () => process.kill(process.pid, "SIGKILL");
try {
  if (fstatSync(0, { bigint: true }).isFIFO()) {
    process.stdin.once("end", hardExit);
    process.stdin.once("close", hardExit);
  }
} catch {}

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", async (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  if (msg.ojServeInfoAck) {
    stopServeInfoRepush();
    return;
  }
  if (msg.rpcReply != null) {
    const p = rpcPending.get(msg.rpcReply);
    if (p) {
      rpcPending.delete(msg.rpcReply);
      if (msg.error != null) p.reject(new Error(msg.error));
      else p.resolve(msg.result ?? null);
    }
    return;
  }
  const { id, hook, args } = msg;
  try {
    const result = await run(hook, args ?? []);
    ctl({ id, result: result ?? null });
  } catch (e) {
    ctl({ id, error: String((e && e.stack) || e) });
  }
});

// The unconditional init-complete signal, in BOTH modes: the RPC listener above
// is registered, so from here every hang is a hook's, not initialization's.
// Serve mode also pushes { ojServeInfo } (state-bearing, ACKed and re-pushed);
// build mode has no push at all, so without this a hanging first hook would
// wait out Rust's whole init deadline blamed on initialization instead of
// failing on the per-call timeout. Rust treats ojInit, ojServeInfo, or the
// first reply — whichever lands first — as initialized.
ojInitDone = true;
ctl({ ojInit: true });
