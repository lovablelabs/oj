// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import http from "node:http";
import https from "node:https";
import {
  existsSync,
  fstatSync,
  mkdirSync,
  readFileSync,
  realpathSync,
  renameSync,
  statSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { createHash } from "node:crypto";
import { readFile, stat as fsStat } from "node:fs/promises";
import { createRequire, isBuiltin } from "node:module";
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, isAbsolute, join, posix, relative, resolve as pathResolve } from "node:path";
import { foldIncludeSnapshot } from "./discovered-deps.mjs";
import { appViteEntry, asNode } from "./vite-config-node.mjs";
import {
  OJ_NATIVE_PLUGIN_NAMES,
  OJ_UNSUPPORTED_PLUGIN_NAMES,
  flattenPlugins,
  isReimplemented,
} from "./native-plugins.mjs";
import { idFilterGlob, idFilterRegExp, loadViteGlobs, viteGlobs } from "./start/vite-globs.mjs";
import {
  applyHtmlTransforms,
  createHtmlProxy,
  isHtmlProxyRE,
  resolveHtmlTransforms,
  transformDevHtml,
} from "./vite-html.mjs";
import readline from "node:readline";
import { AsyncLocalStorage } from "node:async_hooks";
import { format as formatUtil, stripVTControlCharacters } from "node:util";
import { EventEmitter } from "node:events";
import os from "node:os";

// IN-PROCESS mode: oj's embedded engine seeds `globalThis.__ojPluginHost`
// (what used to be argv + spawn env) before importing this module, and the
// bridge globals `__oj_post` / `__oj_rpc` replace the stdio protocol. Under
// plain `node` (the direct-drive unit tests) none of these exist and the
// original stdio layer below stays live.
const ojEngineBoot = globalThis.__ojPluginHost ?? null;
const ojEnginePost = ojEngineBoot ? globalThis.__oj_post : null;
const ojEngineRpc = ojEngineBoot ? globalThis.__oj_rpc : null;

// Parent watchdog (esbuild's --ppid model), armed before anything can await:
// the stdin-EOF net at the bottom of this file only arms after every top-level
// await, and a boot stuck awaiting an RPC its dead parent will never answer
// writes nothing (so no EPIPE crash) and reads nothing (so no EOF) — that host
// survived as an orphan forever. When the spawning oj process dies, the host is
// reparented (ppid flips to the reaper); poll for that and hard-exit. unref'd
// so the timer itself never keeps the event loop alive. In-process there is no
// parent to watch and process.kill would kill OJ ITSELF: never arm it.
if (!ojEngineBoot) {
  const spawnPpid = process.ppid;
  setInterval(() => {
    if (process.ppid !== spawnPpid) process.kill(process.pid, "SIGKILL");
  }, 1000).unref();
}

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
      merged[key] = [...(Array.isArray(existing) ? existing : [existing]), ...(Array.isArray(value) ? value : [value])];
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

const pluginsPath = ojEngineBoot ? ojEngineBoot.pluginsPath : process.argv[2];
const initial = JSON.parse((ojEngineBoot ? ojEngineBoot.initialJson : process.argv[3]) ?? "{}");

// PROCESS ISOLATION for the in-process host, before any plugin or config code
// (and before the NODE_ENV pre-set below) can observe it. The node child kept
// its env writes, cwd and exit to itself; sharing oj's process, they must be
// shadowed instead:
// - `process.env` becomes a plain private copy (Deno.env writes are
//   process-real; a long-lived host mutating oj's env — Vite's own NODE_ENV
//   dance runs right below — is unacceptable, and getEnvDelta stays a diff of
//   this private copy). Same mechanism as the in-process config extractor.
// - `process.cwd()` pins to the app root, where the old child ran (plugins
//   build cwd-relative filters from it). `process.chdir` moves only the
//   shadow: a real chdir would move EVERY engine in the process.
// - `process.exit` really exits, taking oj down: that is Vite's contract
//   (plugins share the server process), and supervisor-style plugins rely on
//   it to force a supervised respawn of a wedged dev server. A stderr line
//   names the caller's intent first, since oj dying without one would read as
//   a crash.
// - stdout is oj's own protocol-free terminal now: the old spawn PIPED the
//   host's stdout and dropped unframed lines, so plugin stdout prints were
//   swallowed; route them (console.log included — Deno's console does not go
//   through process.stdout) to stderr, which was always the visible stream.
if (ojEngineBoot) {
  try {
    Object.defineProperty(process, "env", {
      value: { ...process.env },
      configurable: true,
      writable: true,
    });
  } catch {}
  if (ojEngineBoot.cacheRoot) process.env.OJ_CACHE_ROOT = String(ojEngineBoot.cacheRoot);
  // process.cwd() THROWS NotFound when the process's directory was deleted
  // (another host chdir'd somewhere that vanished), so the boot path must
  // never require it; a config root is always present in practice.
  const configRoot = initial.config?.root;
  const appRoot =
    configRoot ??
    (() => {
      try {
        return process.cwd();
      } catch {
        return "/";
      }
    })();
  // The old spawn ran with cwd = the app root, and plugins depend on it for
  // RELATIVE fs paths (a lifecycle hook writing ".oj-cache/…") as much as for
  // process.cwd() readers. Really chdir — the Start-phase scripts set the
  // same cwd, and oj itself resolves every root to an absolute path at CLI
  // startup, before any engine exists — then contain any LATER chdir to a
  // shadow so a plugin can never move the whole oj process again (its cwd()
  // readers follow the shadow; its relative fs stays anchored at the root —
  // the one divergence from a real child, documented cost of sharing the
  // process).
  // Seed the shadow from the root itself (realpathed, like a real child's
  // cwd), never from a process.cwd() read-back: cwd is process-global, so
  // another engine booting a different root between our chdir and a read
  // would leak ITS root into this host's shadow, and the read itself throws
  // once the process's directory has been deleted. Only a configured root is
  // worth a REAL chdir; a rootless boot must not move every other engine's
  // relative-fs anchor to wherever this one happens to start.
  let shadowCwd = String(appRoot);
  if (configRoot != null) {
    try {
      process.chdir(String(configRoot));
    } catch (e) {
      // The host still boots (the shadow anchors cwd() readers), but relative
      // fs stays wherever the process was; say so instead of failing quietly.
      try {
        process.stderr.write(`${OJ} plugin host: could not chdir to ${configRoot}: ${e?.message ?? e}\n`);
      } catch {}
    }
  }
  try {
    shadowCwd = realpathSync(String(appRoot));
  } catch {}
  try {
    process.cwd = () => shadowCwd;
    process.chdir = (dir) => {
      shadowCwd = pathResolve(shadowCwd, String(dir));
    };
  } catch {}
  try {
    const realExit = process.exit.bind(process);
    process.exit = (code) => {
      try {
        process.stderr.write(`${OJ} plugin host: a plugin called process.exit(${code ?? 0}); exiting\n`);
      } catch {}
      realExit(code ?? 0);
    };
  } catch {}
  try {
    const toStderr = (...args) => {
      try {
        process.stderr.write(args.map((a) => (typeof a === "string" ? a : formatUtil(a))).join(" ") + "\n");
      } catch {}
    };
    for (const name of ["log", "info", "debug", "trace"]) console[name] = toStderr;
    process.stdout.write = (chunk, ...rest) => process.stderr.write(chunk, ...rest);
  } catch {}
}

process.env.VITE_CONFIG_NATIVE_IGNORE_WARNING ??= "true";
// Vite's ConfigEnv (config.ts): `{ mode, command, isSsrBuild, isPreview }`.
// Vite computes it once, before any environment exists, from the inline
// config alone (`command === "build" && !!config.build?.ssr`); oj passes no
// inline build.ssr, and a builder (Start, every environment build) resolves
// one config for all environments, so isSsrBuild is false in every host,
// the ssr one included.
const hostEnvName = initial.environment?.name ?? "client";
const env = { command: "serve", mode: "development", ...(initial.env ?? {}) };
env.isSsrBuild = env.isSsrBuild ?? false;
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
// A Start bundle host backs the bundles of a Start build (client, server,
// workers; the dev client bundle is a container of the dev server's host):
// the bundler is rolldown in Rust, which drives this host's hooks, and a plugin's
// `this.resolve` that no plugin answers ends in that bundler's resolution
// (the `bundlerResolve` reverse RPC), as it ends in Rollup's under Vite.
const ojBundleHost = initial.ojBundleHost === true;
// In start mode framework plugins oj reimplements natively (vite:*,
// tanstack's compiler plugins) must not transform SSR modules or a native
// Start bundle's — the Start pipeline does their work (server-fn rewrite,
// route tree, refresh) — while user plugins (mdx, svgr, virtual modules)
// run. A host of its own for either leaves them out; so does a container
// opened for either on the dev server's host (openEnvironmentContainer).
const startLeavesOutReimplemented = ojStartMode && (hostEnvName === "ssr" || ojBundleHost);

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
    process.stderr.write(
      `${OJ}: experimental.bundledDev is not supported; using the standard dev client entry instead\n`,
    );
  }
  return flipped;
}

// The one writer for every oj control push (ojServeInfo/ojServer/ojWs and the
// init signals). In-process it is the engine's push channel (`__oj_post`): a
// value delivery a plugin's print can never splice into, so no framing exists
// on that path at all. Under plain `node` (the direct-drive unit tests) the
// old stdout line protocol stays: the leading newline terminates any
// unterminated partial line a plugin left on the stream, and the per-session
// token (OJ_CONTROL_TOKEN) marks the line as oj's so the test driver ignores
// unframed lines, exactly as the retired Rust reader did.
const CONTROL_TOKEN = process.env.OJ_CONTROL_TOKEN || "";
function ctl(obj) {
  if (ojEnginePost) {
    ojEnginePost(JSON.stringify(obj));
    return;
  }
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
// that leaves the event loop alive and defeat the evidence. One exception,
// bootPreseedDone: each tick follows Rust's answer that the boot-time deps
// pre-seed still runs, and that child has a kill deadline of its own.
let ojInitDone = false;
const initStage = (stage) => {
  if (!ojInitDone) ctl({ ojInitProgress: stage });
};
initStage("start");

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
      const baseDir = importer ? dirname(importer.startsWith("file://") ? fileURLToPath(importer) : importer) : root;
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

// Vite's searchRoot.ts: the nearest ancestor that looks like a workspace
// root (pnpm-workspace.yaml or lerna.json beside it, a package.json with
// `workspaces`, or a deno.json(c) with `workspace`), else the nearest
// package root. `server.fs.allow` defaults to it.
function searchForPackageRoot(current, root = current) {
  for (;;) {
    if (existsSync(join(current, "package.json"))) return current;
    const dir = dirname(current);
    if (!dir || dir === current) return root;
    current = dir;
  }
}
function searchForWorkspaceRoot(current, root = searchForPackageRoot(current)) {
  const readJson = (p) => {
    try {
      return JSON.parse(readFileSync(p, "utf8")) || {};
    } catch {
      return null;
    }
  };
  for (;;) {
    if (["pnpm-workspace.yaml", "lerna.json"].some((f) => existsSync(join(current, f)))) return current;
    if (readJson(join(current, "package.json"))?.workspaces) return current;
    if (["deno.json", "deno.jsonc"].some((f) => readJson(join(current, f))?.workspace)) return current;
    const dir = dirname(current);
    if (!dir || dir === current) return root;
    current = dir;
  }
}

// Vite's resolved `server` also guarantees these scalars
// (_serverConfigDefaults, server/index.ts): plugins read them exactly the
// way they read fs.allow. Present values always win — including the REAL
// port oj's Rust side passes in the boot config — and only absent fields
// get Vite's defaults (undefined-only, mergeWithDefaults semantics; warmup
// and cors object forms deep-fill). `sourcemapIgnoreList: false` resolves
// to a constant-false function and the default is Vite's isInNodeModules.
function fillResolvedServerScalars(server) {
  // `host` is deliberately NOT filled: Vite overrides its own default with
  // undefined ("do not set here to detect whether host is set or not",
  // server/index.ts) so plugins can tell an explicit host apart.
  const defaults = {
    port: 5173,
    strictPort: false,
    allowedHosts: [],
    open: false,
    middlewareMode: false,
    preTransformRequests: true,
    perEnvironmentStartEndDuringDev: false,
    perEnvironmentWatchChangeDuringDev: false,
    headers: {},
  };
  for (const k of Object.keys(defaults)) {
    if (server[k] === undefined) server[k] = defaults[k];
  }
  const isPlain = (v) => v !== null && typeof v === "object" && !Array.isArray(v);
  server.warmup = isPlain(server.warmup)
    ? { clientFiles: [], ssrFiles: [], ...server.warmup }
    : (server.warmup ?? { clientFiles: [], ssrFiles: [] });
  const corsDefault = { origin: /^https?:\/\/(?:(?:[^:]+\.)?localhost|127\.0\.0\.1|\[::1\])(?::\d+)?$/ };
  server.cors = isPlain(server.cors)
    ? { ...corsDefault, ...server.cors }
    : server.cors === undefined
      ? corsDefault
      : server.cors;
  if (server.sourcemapIgnoreList === false) server.sourcemapIgnoreList = () => false;
  else if (server.sourcemapIgnoreList === undefined) {
    server.sourcemapIgnoreList = (id) => id.includes("node_modules");
  }
  // Vite resolves `forwardConsole` to { enabled, unhandledErrors, logLevels }
  // (resolveForwardConsoleOptions). An unset value takes Vite's disabled
  // shape (its default is agent-detection, but oj does not forward console,
  // so advertising agent-enabled would lie); explicit user values resolve
  // exactly as Vite resolves them.
  const fc = server.forwardConsole;
  // An already-resolved shape (enabled boolean) passes through — including a
  // vite-resolved base config whose agent detection turned it on; oj does not
  // forward console, but rewriting a resolved value the user's own vite run
  // produced would be worse than carrying it.
  if (fc === undefined || fc === false) {
    server.forwardConsole = { enabled: false, unhandledErrors: false, logLevels: [] };
  } else if (fc === true) {
    server.forwardConsole = { enabled: true, unhandledErrors: true, logLevels: ["error", "warn"] };
  } else if (fc !== null && typeof fc === "object" && typeof fc.enabled !== "boolean") {
    const unhandledErrors = fc.unhandledErrors ?? true;
    const logLevels = fc.logLevels ?? [];
    server.forwardConsole = { enabled: unhandledErrors || logLevels.length > 0, unhandledErrors, logLevels };
  }
  // Vite's setupHmrWsOptionCompat (utils.ts): unless hmr or ws is disabled,
  // resolved `server.ws` is ALWAYS an object, with the legacy hmr keys
  // mirrored in where ws leaves them unset; `hmr: true` normalizes to {}.
  // (The deprecation defineProperty shim is warning sugar, not shape.)
  if (server.hmr !== false && server.ws !== false) {
    if (server.hmr === true) server.hmr = {};
    const ws = server.ws ? { ...server.ws } : {};
    if (server.hmr && typeof server.hmr === "object") {
      for (const key of ["protocol", "host", "port", "clientPort", "path", "timeout", "server"]) {
        if (server.hmr[key] !== undefined) ws[key] ??= server.hmr[key];
      }
    }
    server.ws = ws;
  }
  // Vite appends __VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS (comma-split,
  // trimmed) unless it carries reserved characters (server/index.ts).
  const rawAdditionalHosts = process.env.__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS;
  if (rawAdditionalHosts && Array.isArray(server.allowedHosts) && !/[\\"']/.test(rawAdditionalHosts)) {
    server.allowedHosts = [
      ...server.allowedHosts,
      ...rawAdditionalHosts
        .split(",")
        .map((h) => h.trim())
        .filter(Boolean),
    ];
  }
  // Vite strips a single trailing slash off a user `origin`.
  if (typeof server.origin === "string" && server.origin.endsWith("/")) {
    server.origin = server.origin.slice(0, -1);
  }
  return server;
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
  // Vite's resolved config ALWAYS carries an absolute `cacheDir`
  // (config.ts: user value resolved from root, else `node_modules/.vite`
  // beside the nearest package.json, else `<root>/node_modules/.vite` when
  // that dir exists, else `<root>/.vite`); plugins path.join() it directly
  // and an undefined here throws in their configResolved.
  // Vite resolves root first (`path.resolve(config.root)`), so a relative
  // root from a config hook still yields an absolute cacheDir.
  const cacheRoot = pathResolve(merged.root);
  if (typeof merged.cacheDir !== "string" || merged.cacheDir.length === 0) {
    // findNearestPackageData: a package.json that does not parse is skipped
    // and the walk continues upward.
    const nearestPkgDir = (() => {
      let dir = cacheRoot;
      for (;;) {
        const pkg = join(dir, "package.json");
        try {
          if (statSync(pkg).isFile()) {
            JSON.parse(readFileSync(pkg, "utf8"));
            return dir;
          }
        } catch {}
        const parent = dirname(dir);
        if (parent === dir) return null;
        dir = parent;
      }
    })();
    merged.cacheDir = nearestPkgDir
      ? join(nearestPkgDir, "node_modules/.vite")
      : existsSync(join(cacheRoot, "node_modules"))
        ? join(cacheRoot, "node_modules/.vite")
        : join(cacheRoot, ".vite");
  } else if (!isAbsolute(merged.cacheDir)) {
    merged.cacheDir = pathResolve(cacheRoot, merged.cacheDir);
  }
  // Vite resolves `publicDir` to an absolute path (default `<root>/public`);
  // plugins like @crxjs read it directly to locate manifest assets (icons,
  // locales). `false` disables it, matching Vite.
  if (merged.publicDir !== false) {
    const pd = typeof merged.publicDir === "string" && merged.publicDir.length > 0 ? merged.publicDir : "public";
    merged.publicDir = isAbsolute(pd) ? pd : pathResolve(merged.root, pd);
  }
  // Vite ALWAYS resolves `server.fs` (resolveServerOptions): strict on, the
  // default deny list, and `allow` defaulting to the workspace root, each
  // entry resolved absolute. Plugins index into `config.server.fs.allow`
  // directly in resolveId-time checks (isServeableFile shapes), so an
  // undefined here fails every bundle that runs them. Vite's pnpm
  // virtual-store addition is not mirrored: it reads
  // node_modules/.modules.yaml with JSON.parse (server/index.ts), which
  // throws on pnpm's real YAML and is swallowed, so observed Vite behavior
  // carries no store entry either. Vite's own-client-dir push (CLIENT_DIR
  // when vite sits outside every allow entry) has no oj equivalent: oj
  // serves its client from memory, and its serving side allows any
  // node_modules path outright.
  {
    merged.server = merged.server ?? {};
    const rawFs = merged.server.fs ?? {};
    // Vite: `allow: raw?.fs?.allow ?? [workspaceRoot]` — an EXPLICIT empty
    // list stays empty; only an absent one gets the workspace-root default.
    const allow = Array.isArray(rawFs.allow) ? rawFs.allow : [searchForWorkspaceRoot(cacheRoot)];
    merged.server.fs = {
      ...rawFs,
      // mergeWithDefaults skips only `undefined` ("let null to set the
      // value"), so an explicit null passes through as in Vite.
      strict: rawFs.strict === undefined ? true : rawFs.strict,
      // Vite's normalizePath backslash-replaces on Windows only; POSIX
      // filenames may legally contain one.
      allow: allow.map((d) =>
        process.platform === "win32" ? pathResolve(cacheRoot, d).replace(/\\/g, "/") : pathResolve(cacheRoot, d),
      ),
      deny:
        rawFs.deny === undefined
          ? [".env", ".env.*", "*.{crt,pem,key,p12,pfx,cer,der}", ".npmrc", ".yarnrc.yml", "**/.git/**"]
          : rawFs.deny,
    };
    fillResolvedServerScalars(merged.server);
  }
  // Vite's resolved config carries a `logger`; plugins (e.g. the cloudflare
  // plugin's ViteMiniflareLogger) call `config.logger.info/warn/error`.
  if (!merged.logger || typeof merged.logger.info !== "function") {
    const w = (...a) => process.stderr.write(a.map(String).join(" ") + "\n");
    merged.logger = {
      info: () => {},
      warn: w,
      warnOnce: w,
      error: w,
      clearScreen: () => {},
      hasErrorLogged: () => false,
      hasWarned: false,
    };
  }
  return merged;
}

// The environment the host's hooks run in. A Start build's host serves every
// environment in turn on the same plugin instances (Vite's
// `builder.sharedPlugins`): `switchEnvironment` moves it to the next one.
const environment = {
  name: hostEnvName,
  mode: initial.environment?.mode ?? env.mode,
  config: withResolvedDefaults(
    mergeConfigLite(initial.config ?? {}, (initial.config?.environments ?? {})[hostEnvName] ?? {}),
  ),
};
// Vite tags each environment's resolved config with a `consumer` ("client" or
// "server"); plugins like @vitejs/plugin-react read `env.config.consumer`
// directly in applyToEnvironment, so it must be present or they throw.
environment.config.consumer = environment.config.consumer ?? (hostEnvName === "client" ? "client" : "server");
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
function transformRequestFor(c) {
  const transformRequest = async (rawId) => {
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
    const loaded = await loadFull(c, bareId);
    if (loaded && loaded.code != null) {
      source = loaded.code;
    } else {
      const root = c.resolvedConfig?.root ?? c.environment.config?.root ?? process.cwd();
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
        const r = await contextOf(c).resolve(bareId, undefined);
        if (r && r.id && r.id !== bareId) return transformRequest(r.id + query);
        return null;
      }
    }
    const out = JSON.parse(await transform(c, source, bareId + query, null));
    return { code: out.code };
  };
  return transformRequest;
}
// The host's own plugin container, over the environment above. Its resolved
// config (defaults + plugin `config` hooks) is what configureServer receives,
// not the raw initial.config, so plugins reading resolved-only fields
// (experimental, environments, plugins) don't throw; it is seeded with the
// resolved environment config until runConfigHooks recomputes it.
const main = createContainer(0, environment, environment.config);
environment.transformRequest = transformRequestFor(main);

// When the app has Vite installed and its config is a vite.config, ask that Vite
// to resolve the user's config once and use the result as the base the plugins
// see: real build.outDir/sourcemap/rollupOptions, cacheDir, env, envPrefix,
// assetsInclude(), css, ssr, worker, createResolver, logger. Like Vite's
// createServer, the file is evaluated once: the resolve runs over the config
// loadViteConfig already loaded and the host's own plugin instances, each
// config hook running once under Vite with its result recorded for
// runConfigHooks to replay (the later config-phase hooks run only in the
// host). Without Vite the synthesized defaults stand.
let userResolvedViteConfig = null;
// What each hosted plugin's config hook did inside Vite's resolve.
const recordedConfigPartials = new Map();
// What each plugin's configEnvironment hook did, by environment.
const recordedEnvironmentPartials = new Map();
// The app's Vite, for a resolve over the user's config.
async function appViteForResolve() {
  if (initial.pluginsFormat !== "vite" || !userViteConfig) return null;
  try {
    const vite = await import(appViteEntry(initial.config?.root ?? process.cwd()));
    return typeof vite.resolveConfig === "function" ? vite : null;
  } catch {
    return null;
  }
}
// The recording resolve: the config hooks run once, on the host's own
// instances, under Vite, each recorded for the host to replay.
async function loadUserResolvedViteConfig() {
  const vite = await appViteForResolve();
  if (!vite) return null;
  // A Start SSR or bundle host resolves without the framework plugins oj
  // reimplements, as it runs without them: their config hooks (the router
  // generator's among them) do work oj's own pipeline does.
  const candidates = startLeavesOutReimplemented ? allPlugins.filter((p) => !isReimplemented(p.name)) : allPlugins;
  const hosted = new Set(main.plugins);
  const restores = [];
  // Through the config hooks the hidden hooks still read as the plugin's own
  // (a sibling may wrap one); Vite reads them only after, by when they hide.
  const phase = { hidden: false };
  const plugins = candidates.map((p) => {
    if (!hosted.has(p)) return recordingConfig(p);
    const restore = recordInPlace(p, phase);
    if (!restore) return recordingPlugin(p);
    restores.push(restore);
    return p;
  });
  // The file under oj's CLI-level values, as resolveConfig merges a loaded
  // file under the inline config before any config hook runs.
  const fileConfig = plainCopy(userViteConfig);
  // Vite's configEnv reads `isSsrBuild` off the inline config alone, before
  // the file loads: a build's `build.ssr` from the file joins the config
  // through the first config hook instead, so the hooks see it in the config
  // but not in configEnv.
  const fileSsr = env.command === "build" ? fileConfig.build?.ssr : undefined;
  // The plugins this resolve adds for itself.
  const own = new Set();
  if (fileSsr) {
    fileConfig.build = { ...fileConfig.build };
    delete fileConfig.build.ssr;
    const fileBuildSsr = {
      name: "oj:file-build-ssr",
      enforce: "pre",
      config: { order: "pre", handler: () => ({ build: { ssr: fileSsr } }) },
    };
    own.add(fileBuildSsr);
    plugins.unshift(fileBuildSsr);
  }
  const hooksDone = {
    name: "oj:config-hooks-done",
    enforce: "post",
    config: {
      order: "post",
      handler() {
        phase.hidden = true;
      },
    },
  };
  own.add(hooksDone);
  plugins.push(hooksDone);
  try {
    const rc = await viteResolve(vite, inlineMerge(vite)(fileConfig, cliInlineConfig()), plugins);
    // oj's own plugins leave the resolved config, absent as from the app's
    // own resolve: Vite hashes the plugin names into each deps optimizer's
    // configHash, and the preseed child's metadata was hashed without them.
    if (rc) withoutPlugins(rc, (p) => own.has(p));
    return rc;
  } finally {
    for (const restore of restores) {
      try {
        restore();
      } catch {}
    }
  }
}
// `rc` (a resolved config) without the plugins `drop` matches, in place.
function withoutPlugins(rc, drop) {
  const kept = (list) => {
    if (!Array.isArray(list)) return;
    for (let i = list.length - 1; i >= 0; i--) if (drop(list[i])) list.splice(i, 1);
  };
  kept(rc.plugins);
  for (const options of Object.values(rc.environments ?? {})) kept(options?.plugins);
}
// A replay resolve: the app's Vite over `plugins`, each answering what its
// hooks did the first time, under the same inline config.
async function resolveUserViteConfig(plugins) {
  const vite = await appViteForResolve();
  if (!vite) return null;
  return viteResolve(vite, inlineMerge(vite)(plainCopy(userViteConfig), cliInlineConfig()), plugins);
}
const inlineMerge = (vite) => (typeof vite.mergeConfig === "function" ? vite.mergeConfig : mergeConfigLite);
async function viteResolve(vite, base, plugins) {
  const appRoot = initial.config?.root ?? process.cwd();
  try {
    // Vite's own NODE_ENV handling runs inside resolveConfig; hand it the
    // command-based defaultNodeEnv (never the mode) and lift oj's untouched
    // pre-set first so it computes isNodeEnvSet like a real CLI run.
    unsetOjNodeEnvForResolve();
    const rc = await asNode(() =>
      vite.resolveConfig(
        { ...base, plugins, root: appRoot, configFile: false, mode: env.mode, logLevel: "silent" },
        env.command,
        env.mode,
        defaultNodeEnv,
      ),
    );
    if (!rc || typeof rc !== "object") return null;
    // What a resolve that loaded the file itself reports.
    rc.configFile = pluginsPath;
    rc.configFileDependencies = userConfigDependencies;
    rc.inlineConfig = { root: appRoot, configFile: pluginsPath, mode: env.mode, logLevel: "silent" };
    return rc;
  } catch (e) {
    process.stderr.write(
      `${OJ} plugin host: vite.resolveConfig failed, using synthesized config: ${(e && e.message) || e}\n`,
    );
    return null;
  } finally {
    // A throwing resolveConfig may leave the unset behind; the fallback paths
    // below still evaluate config/plugin code that must see a value.
    if (!process.env.NODE_ENV) process.env.NODE_ENV = defaultNodeEnv;
  }
}
// What oj's command line set (`--port`, `--host`), as a Vite CLI run puts
// it in the inline config: the config hooks see it over the file's values.
function cliInlineConfig() {
  const cli = initial.cliInline;
  return cli && typeof cli === "object" ? plainCopy(cli) : {};
}
// oj's values without the command line's, which Vite's resolve already took
// in under the config hooks (a hook's in-place edit of one stands). The rest
// (base, the bound port when none was passed) overlay the resolved config,
// so the host serves what oj serves.
function withoutCliInline(o) {
  const cliServer = cliInlineConfig().server;
  if (!cliServer || !o?.server) return o ?? {};
  const kept = { ...o.server };
  for (const k of Object.keys(cliServer)) delete kept[k];
  return { ...o, server: kept };
}
// The resolve's stand-ins, by the plugin each stands in for.
const standInFor = new WeakMap();
// `p` with its config hook in place of `config` (and `hidden` over the rest).
function withConfigHook(p, handler, hidden) {
  const hook = p.config;
  const fields = { ...hidden };
  if (hookFn(p, "config")) fields.config = { value: typeof hook === "function" ? handler : { ...hook, handler } };
  const standIn = Object.create(p, fields);
  standInFor.set(standIn, p);
  return standIn;
}
// What a config-phase hook did: the partial it returned, and its in-place
// edits of the object it was handed (Vite honors both: runConfigHook merges
// the partial into that very object).
function hookRecord(before, after, returned) {
  return { partial: returned === after ? null : plainCopy(returned), edits: editsOf(before, after) };
}
const DELETED = Symbol("deleted");
function isPlainData(v) {
  if (!v || typeof v !== "object" || Array.isArray(v)) return false;
  const proto = Object.getPrototypeOf(v);
  return proto === Object.prototype || proto === null;
}
function sameValue(a, b) {
  if (Object.is(a, b)) return true;
  if (Array.isArray(a) && Array.isArray(b)) return a.length === b.length && a.every((x, i) => sameValue(x, b[i]));
  if (isPlainData(a) && isPlainData(b)) {
    const keys = Object.keys(a);
    return keys.length === Object.keys(b).length && keys.every((k) => k in b && sameValue(a[k], b[k]));
  }
  return false;
}
// An in-place array edit that kept what the hook saw, in order, between new
// items: replayed around the replay's own array, so items another plugin put
// there earlier (one the replay leaves out) stay out.
class Wrapped {
  constructor(head, tail) {
    this.head = head;
    this.tail = tail;
  }
}
function arrayEdit(before, after) {
  for (let at = 0; at + before.length <= after.length; at++) {
    if (before.every((x, i) => sameValue(x, after[at + i]))) {
      return new Wrapped(plainCopy(after.slice(0, at)), plainCopy(after.slice(at + before.length)));
    }
  }
  return plainCopy(after);
}
// The edits that turn `before` (a plainCopy) into `after`, as [path, value]
// pairs (DELETED for a removed key). The plugin array is the resolve's own.
function editsOf(before, after, path = [], out = []) {
  if (!isPlainData(after)) return out;
  const had = isPlainData(before) ? before : {};
  for (const key of Object.keys(after)) {
    if (path.length === 0 && key === "plugins") continue;
    const a = had[key];
    const b = after[key];
    if (isPlainData(a) && isPlainData(b)) editsOf(a, b, [...path, key], out);
    else if (Array.isArray(a) && Array.isArray(b)) {
      if (!sameValue(a, b)) out.push([[...path, key], arrayEdit(a, b)]);
    } else if (!(key in had) || !sameValue(a, b)) out.push([[...path, key], plainCopy(b)]);
  }
  for (const key of Object.keys(had)) {
    if (!(key in after) && !(path.length === 0 && key === "plugins")) out.push([[...path, key], DELETED]);
  }
  return out;
}
function applyEdits(target, edits) {
  // One array reached under two names (Vite aliases build.rollupOptions to
  // rolldownOptions) carries the edit under both: it is made once.
  const wrapped = new Map();
  for (const [path, value] of edits ?? []) {
    let at = target;
    for (const key of path.slice(0, -1)) {
      if (!isPlainData(at[key])) at[key] = {};
      at = at[key];
    }
    const last = path[path.length - 1];
    if (value === DELETED) delete at[last];
    else if (value instanceof Wrapped) {
      const own = Array.isArray(at[last]) ? at[last] : [];
      if (!wrapped.has(own)) {
        const out = [...plainCopy(value.head), ...own, ...plainCopy(value.tail)];
        wrapped.set(own, out).set(out, out);
      }
      at[last] = wrapped.get(own);
    } else at[last] = plainCopy(value);
  }
  return target;
}
// The edits as a partial (what they set), for what the plugins contributed.
const editsPartial = (edits) =>
  applyEdits(
    {},
    (edits ?? []).filter(([, v]) => v !== DELETED),
  );
const hostRunHooks = {
  configEnvironment: { value: undefined },
  configResolved: { value: undefined },
  applyToEnvironment: { value: undefined },
};
// What `p`'s configEnvironment hook did for each environment.
function environmentPartials(p) {
  if (!recordedEnvironmentPartials.has(p)) recordedEnvironmentPartials.set(p, new Map());
  return recordedEnvironmentPartials.get(p);
}
// `p`'s config hook, recording what it did (its failure too: a hook runs
// once, and the host takes its failure as it took its answer).
function recordingConfigHook(p, fn) {
  return async function (config, configEnv) {
    const before = plainCopy(config);
    let partial;
    try {
      partial = await fn.call(this, config, configEnv);
    } catch (e) {
      recordedConfigPartials.set(p, { ...hookRecord(before, config, null), threw: e });
      throw e;
    }
    recordedConfigPartials.set(p, hookRecord(before, config, partial));
    return partial;
  };
}
// The hooks `p` shows Vite's resolve: its config and configEnvironment hooks
// recorded (the host replays their answers, and the environments Vite
// resolves carry them), its configResolved and applyToEnvironment hidden,
// left to the host, which runs them on its own resolved config.
function resolveHooks(p) {
  const hook = p.config;
  const fn = hookFn(p, "config");
  const record = recordingConfigHook(p, fn);
  const envHook = p.configEnvironment;
  const envFn = hookFn(p, "configEnvironment");
  const recordEnvironment = async function (name, options, configEnv) {
    const before = plainCopy(options);
    let r;
    try {
      r = await envFn.call(this, name, options, configEnv);
    } catch (e) {
      environmentPartials(p).set(name, { ...hookRecord(before, options, null), threw: e });
      throw e;
    }
    environmentPartials(p).set(name, hookRecord(before, options, r));
    return r;
  };
  return {
    config: fn ? (typeof hook === "function" ? record : { ...hook, handler: record }) : undefined,
    configEnvironment: envFn
      ? typeof envHook === "function"
        ? recordEnvironment
        : { ...envHook, handler: recordEnvironment }
      : undefined,
    configResolved: undefined,
    applyToEnvironment: undefined,
  };
}
// Swaps `p`'s hooks for the resolve's on the instance itself, so the plugin
// list Vite hands the config hooks holds the real objects (a sibling lookup
// or identity check sees the plugin the host runs). Returns the undo, or
// null when `p` cannot take the swap (frozen).
function recordInPlace(p, phase) {
  const hooks = resolveHooks(p);
  const undo = [];
  const restore = () => {
    for (const u of undo.reverse()) {
      try {
        u();
      } catch {}
    }
  };
  try {
    for (const [key, value] of Object.entries(hooks)) {
      const own = Object.getOwnPropertyDescriptor(p, key);
      const enumerable = own?.enumerable ?? false;
      const original = p[key];
      // A hidden hook reads as unset to Vite (which reads it after the config
      // hooks); one the plugin assigns meanwhile (a config hook giving itself
      // a configResolved, or wrapping a sibling's) is held back for the host,
      // which runs it once, as Vite would.
      let assigned;
      let wasAssigned = false;
      const hidden = {
        get: () => (phase.hidden ? undefined : wasAssigned ? assigned : original),
        set: (v) => {
          assigned = v;
          wasAssigned = true;
        },
      };
      const swapped =
        value === undefined
          ? { ...hidden, configurable: true, enumerable }
          : { value, writable: true, configurable: true, enumerable };
      Object.defineProperty(p, key, swapped);
      undo.push(() => {
        const now = Object.getOwnPropertyDescriptor(p, key);
        if (wasAssigned) {
          Object.defineProperty(p, key, { value: assigned, writable: true, configurable: true, enumerable });
        } else if (now && (now.get === hidden.get || (value !== undefined && now.value === value))) {
          if (own) Object.defineProperty(p, key, own);
          else delete p[key];
        }
        // Otherwise the plugin redefined the hook itself: it keeps that.
      });
    }
  } catch {
    restore();
    return null;
  }
  return restore;
}
// `p` behind a wrapper carrying the resolve's hooks, for a plugin that
// cannot take them in place (frozen: no sibling can wrap its hooks either).
function recordingPlugin(p) {
  const hooks = resolveHooks(p);
  const standIn = Object.create(p, Object.fromEntries(Object.entries(hooks).map(([k, value]) => [k, { value }])));
  standInFor.set(standIn, p);
  return standIn;
}
// A plugin the host does not run (one oj does natively), its config hook
// recorded and the rest left to Vite.
function recordingConfig(p) {
  const fn = hookFn(p, "config");
  if (!fn) return p;
  return withConfigHook(p, recordingConfigHook(p, fn));
}
// `p` answering its config hook with what it did the first time, for a
// resolve that must not run any hook twice; with `environments` its
// configEnvironment answers too.
function replayingPlugin(p, environments = false) {
  const hidden = { ...hostRunHooks };
  if (environments && hookFn(p, "configEnvironment")) {
    const replay = (name, options) => {
      const record = recordedEnvironmentPartials.get(p)?.get(name);
      applyEdits(options, record?.edits);
      return record?.partial ?? null;
    };
    const hook = p.configEnvironment;
    hidden.configEnvironment = { value: typeof hook === "function" ? replay : { ...hook, handler: replay } };
  }
  const replay = (config) => {
    const record = recordedConfigPartials.get(p);
    applyEdits(config, record?.edits);
    return record?.partial ?? null;
  };
  return withConfigHook(p, replay, hidden);
}
// A copy of `v`'s plain data (object literals and arrays); functions, RegExps
// and class instances are kept by reference, and so are the plugins of a
// `plugins` list: a copy of a plugin is another instance.
function plainCopy(v, seen = new WeakMap()) {
  if (!v || typeof v !== "object") return v;
  const cached = seen.get(v);
  if (cached) return cached;
  if (Array.isArray(v)) {
    const out = [];
    seen.set(v, out);
    for (const x of v) out.push(plainCopy(x, seen));
    return out;
  }
  const proto = Object.getPrototypeOf(v);
  if (proto !== Object.prototype && proto !== null) return v;
  const out = {};
  seen.set(v, out);
  for (const [k, x] of Object.entries(v)) out[k] = k === "plugins" ? pluginListCopy(x) : plainCopy(x, seen);
  return out;
}
function pluginListCopy(v) {
  return Array.isArray(v) ? v.map(pluginListCopy) : v;
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

// Rolldown twins of the two esbuild config-bundling plugins above (the same
// two byte-identical copies again): an app whose vite is rolldown-vite ships
// rolldown, not esbuild, so the direct config bundle must speak both APIs.
// Semantics mirror the esbuild pair exactly -- resolve every bare import from
// its importer, keep builtins/npm: bare-external, externalize resolved
// node_modules deps as file:// URLs, and bundle TS sources, first-party files
// and .json inline -- in the hook shapes rolldown-vite's own bundleConfigFile
// installs (a filtered `resolveId` and a filtered `transform`).
function externalizeDepsRolldownPlugin() {
  const warned = new Set();
  return {
    name: "externalize-deps",
    resolveId: {
      filter: { id: /^[^./#]/ },
      async handler(id, importer) {
        if (!importer || isAbsolute(id)) return null;
        if (id.startsWith("node:") || id.startsWith("bun:") || isBuiltin(id)) {
          return { id, external: true };
        }
        // npm: specifiers stay bare-external (Vite keeps them bare too); any
        // other scheme-shaped id (data:, ...) is the bundler's to handle
        // natively -- pathToFileURL on those would produce garbage.
        if (id.startsWith("npm:")) return { id, external: true };
        if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(id)) return null;
        let resolved = null;
        try {
          // skipSelf defaults on: this.resolve never re-enters this hook.
          resolved = await this.resolve(id, importer);
        } catch {}
        if (!resolved || !resolved.id) {
          // Unresolvable here; keep the bare id external (see the esbuild
          // twin: it may still resolve at import time), saying so once.
          if (!warned.has(id)) {
            warned.add(id);
            process.stderr.write(
              `oj: vite.config: could not resolve "${id}" imported from ${importer}; kept as a bare import that must resolve when the bundled config loads\n`,
            );
          }
          return { id, external: true };
        }
        if (resolved.external) return { id: resolved.id, external: true };
        // Another resolver's virtual namespace is not ours to externalize.
        if (resolved.id.startsWith("\0")) return null;
        if (
          /\.(?:ts|tsx|mts|cts)$/.test(resolved.id) ||
          !/[\\/]node_modules[\\/]/.test(resolved.id) ||
          resolved.id.endsWith(".json")
        ) {
          return resolved.id;
        }
        return { id: pathToFileURL(resolved.id).href, external: true };
      },
    },
  };
}
function injectFileScopeVariablesRolldownPlugin() {
  return {
    name: "inject-file-scope-variables",
    transform: {
      filter: { id: /\.[cm]?[jt]sx?$/ },
      handler(code, id) {
        const inject =
          `const __vite_injected_original_dirname = ${JSON.stringify(dirname(id))};` +
          `const __vite_injected_original_filename = ${JSON.stringify(id)};` +
          `const __vite_injected_original_import_meta_url = ${JSON.stringify(pathToFileURL(id).href)};`;
        let injected;
        if (code.startsWith("#!")) {
          const nl = code.indexOf("\n");
          injected = nl === -1 ? code + "\n" + inject : code.slice(0, nl + 1) + inject + code.slice(nl + 1);
        } else {
          injected = inject + code;
        }
        return { code: injected, map: null };
      },
    },
  };
}

// Vite's bundleConfigFile, for whichever bundler the app's vite lineage ships
// (the same two byte-identical copies again): stock Vite bundles configs with
// esbuild, rolldown-vite with rolldown, and an app only carries the one its
// vite uses -- hard-requiring esbuild turned a rolldown-vite app's config
// failure into "Cannot find module 'esbuild'" (#215). Bundler SELECTION lives
// in here so both shipped copies pick identically: the bundler vite itself
// declares as a dependency wins over whichever happens to be installed (a
// rolldown-vite app with a hoisted esbuild must bundle the way its own
// `vite dev` would), and a missing preferred bundler falls back to the other.
// Returns { code, deps } (deps as the bundler reports them, resolvable
// against the app root), or null when NO bundler resolves -- the caller owns
// that error.
async function bundleViteConfigFile(configPath, appRoot) {
  const req = createRequire(appRoot + "/package.json");
  let vitePkgDir = null;
  try {
    vitePkgDir = dirname(req.resolve("vite/package.json"));
  } catch {}
  // The bundlers are vite's dependencies, not usually the app's own.
  const resolveSpec = (spec) => {
    try {
      return req.resolve(spec);
    } catch {}
    if (vitePkgDir) {
      try {
        return req.resolve(spec, { paths: [vitePkgDir] });
      } catch {}
    }
    return null;
  };
  let viteDeps = null;
  if (vitePkgDir) {
    try {
      viteDeps = JSON.parse(readFileSync(vitePkgDir + "/package.json", "utf8")).dependencies ?? null;
    } catch {}
  }
  const preferRolldown = !!(viteDeps && viteDeps.rolldown);
  let esbuildPath = preferRolldown ? null : resolveSpec("esbuild");
  let rolldownPath = esbuildPath ? null : resolveSpec("rolldown");
  if (!esbuildPath && !rolldownPath) esbuildPath = resolveSpec("esbuild");
  if (esbuildPath) {
    const esbuild = await import(pathToFileURL(esbuildPath).href);
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
      metafile: true,
      logLevel: "silent",
      absWorkingDir: appRoot,
      define: CONFIG_BUNDLE_DEFINES,
    });
    return { code: result.outputFiles[0].text, deps: Object.keys(result.metafile?.inputs ?? {}) };
  }
  if (!rolldownPath) return null;
  const { rolldown } = await import(pathToFileURL(rolldownPath).href);
  const bundle = await rolldown({
    input: configPath,
    platform: "node",
    cwd: appRoot,
    // Node's resolver shape, as above (platform "node" already pins the
    // "node" condition in rolldown).
    resolve: { mainFields: ["main"] },
    transform: { define: CONFIG_BUNDLE_DEFINES },
    treeshake: false,
    // No tsconfig discovery for config files, as in Vite's bundleConfigFile.
    tsconfig: false,
    logLevel: "silent",
    plugins: [externalizeDepsRolldownPlugin(), injectFileScopeVariablesRolldownPlugin()],
  });
  let output;
  try {
    ({ output } = await bundle.generate({ format: "esm", sourcemap: false, codeSplitting: false }));
  } finally {
    await bundle.close();
  }
  const entry = output.find((c) => c.type === "chunk" && c.isEntry);
  return {
    code: entry.code,
    deps: output.flatMap((c) => (c.type === "chunk" ? c.moduleIds : [])).filter((m) => !m.startsWith("\0")),
  };
}

// The files the config evaluation read (Vite's configFileDependencies).
let userConfigDependencies = [];
async function loadViteConfig(configPath) {
  const appRoot = initial.config?.root ?? process.cwd();
  let vite = null;
  try {
    vite = await import(appViteEntry(appRoot));
  } catch (e) {
    process.stderr.write(`${OJ}${_ojTTY ? "" : ":"} vite unavailable (${e}); bundling config directly\n`);
  }
  if (vite && typeof vite.loadConfigFromFile === "function") {
    // From here on a throw is the config's OWN failure: vite bundled the file
    // with its own bundler and ran it, exactly as `vite dev` would have.
    // Re-bundling re-runs the same code into the same error -- and when the
    // fallback bundler is missing, buries it under a bundler error ("Cannot
    // find module 'esbuild'", #215) -- so propagate it instead, as Vite's
    // loadConfigFromFile itself rethrows.
    const loaded = await asNode(() =>
      vite.loadConfigFromFile(
        // Vite's configEnv (resolveConfig), in its key order.
        { mode: env.mode, command: env.command, isSsrBuild: env.isSsrBuild, isPreview: env.isPreview },
        configPath,
        appRoot,
      ),
    );
    if (loaded && loaded.config) {
      userConfigDependencies = loaded.dependencies ?? [];
      return loaded.config;
    }
  }
  let mod;
  if (/\.(ts|tsx|mts|cts)$/.test(configPath)) {
    const bundled = await bundleViteConfigFile(configPath, appRoot);
    if (!bundled) {
      throw new Error(
        "cannot bundle the TS vite.config: neither esbuild nor rolldown is installed (the app's vite ships one of them)",
      );
    }
    // A unique path per process: the config extractor bundles the same config
    // into this directory concurrently at boot, and two writers on one filename
    // made either importer see a truncated bundle.
    const out = join(
      dirname(fileURLToPath(import.meta.url)),
      `oj-vite-config-${process.pid}-${Math.random().toString(36).slice(2)}.tmp.mjs`,
    );
    writeFileSync(out, bundled.code);
    try {
      mod = await import(pathToFileURL(out).href);
    } finally {
      try {
        unlinkSync(out);
      } catch {}
    }
  } else {
    mod = await import(pathToFileURL(configPath).href);
  }
  return typeof mod.default === "function" ? await mod.default(env) : mod.default;
}

let allPlugins = [];
// The React-family plugins oj reimplements natively. They run no hooks here but
// stay visible in the resolved `config.plugins` (Vite lists every applied
// plugin), so compat probes for vite:react-babel / vite:react-swc find them.
let nativePlugins = [];
const enforceRank = (p) => (p.enforce === "pre" ? -1 : p.enforce === "post" ? 1 : 0);
// Unsupported plugins already reported: every worker bundle hosts its
// `worker.plugins` afresh, and one line per plugin is enough.
const unsupportedReported = new Set();
// The plugins of `list` this host runs, in enforce order: oj's native
// reimplementations and the dev tooling it does not host left out, then
// Vite's filterPlugin, `apply({ ...config, mode }, configEnv)` where config is
// the user's loaded config file merged with the inline config (oj's values,
// root, base, define and server, overlay the file's).
function hostedPlugins(list, userConfig, c = main) {
  let out = list.filter((p) => !ojRunsNatively(p, c));
  out = out.filter((p) => {
    if (OJ_UNSUPPORTED_PLUGIN_NAMES.has(p && p.name)) {
      if (unsupportedReported.has(p.name)) return false;
      unsupportedReported.add(p.name);
      process.stderr.write(
        `${OJ} plugin host: skipping unsupported plugin "${p.name}" (dev-only tooling oj does not host; your app's output is unaffected)\n`,
      );
      return false;
    }
    return true;
  });
  const { plugins: _userPlugins, ...userConfigRest } = userConfig ?? {};
  const applyConfig = { ...mergeConfigLite(userConfigRest, initial.config ?? {}), mode: env.mode };
  out = out.filter((p) => {
    if (p.apply == null) return true;
    try {
      if (typeof p.apply === "function") return !!p.apply(applyConfig, env);
      return p.apply === env.command;
    } catch (e) {
      if (ojStartMode) return true;
      throw e;
    }
  });
  return out.sort((a, b) => enforceRank(a) - enforceRank(b));
}
// Whether oj does `p`'s work itself, so this host leaves it out: the
// top-level list and what applyToEnvironment returns alike.
function ojRunsNatively(p, c = main) {
  const name = p && p.name;
  return OJ_NATIVE_PLUGIN_NAMES.has(name) || (c.leaveOut && isReimplemented(name));
}
// Vite's picomatch, for plugin `filter` ids.
await loadViteGlobs(initial.config?.root ?? process.cwd());
try {
  let list;
  let userConfig = null;
  if (initial.pluginsFormat === "vite") {
    const cfg = await loadViteConfig(pluginsPath);
    userConfig = cfg && typeof cfg === "object" ? cfg : null;
    userViteConfig = userConfig;
    userResolveAlias = cfg?.resolve?.alias ?? null;
    list = cfg?.plugins;
  } else {
    const mod = await import(pathToFileURL(pluginsPath).href);
    list = mod.default ?? mod.plugins;
  }
  allPlugins = await flattenPlugins(list);
  nativePlugins = allPlugins.filter((p) => OJ_NATIVE_PLUGIN_NAMES.has(p && p.name));
  main.rawPlugins = allPlugins;
  main.plugins = hostedPlugins(allPlugins, userConfig);
} catch (e) {
  process.stderr.write(`${OJ} plugin host: failed to load ${pluginsPath}: ${(e && e.stack) || e}\n`);
}
initStage("plugins-loaded");

let rpcCounter = 1;
const rpcPending = new Map();
// The reverse ctx-RPC (oj resolves an id, serves module info). In-process it
// is a SYNCHRONOUS bridge call answered on this same thread — the async shape
// is kept because callers await it. Under plain `node` the old id/reply
// protocol over stdio stays for the direct-drive unit tests.
function ctxRpc(method, args) {
  if (ojEngineRpc) {
    try {
      const raw = ojEngineRpc(method, JSON.stringify(args ?? []));
      return Promise.resolve(raw == null ? null : JSON.parse(raw));
    } catch (e) {
      return Promise.reject(e);
    }
  }
  return asyncRpc(method, args);
}
// The reverse RPC Rust answers later, by the `rpcReply` hook: a push of
// `{ rpc, method, args, container }`. Compiling an inline style runs oj's CSS
// pipeline, which calls this host's own transform hooks, so it cannot ride the
// synchronous bridge that holds the isolate until it returns. `container`
// names whose bundle asks (a worker bundle's `this.resolve` ends in its own
// bundler).
function asyncRpc(method, args, container = 0) {
  const rpc = rpcCounter++;
  return new Promise((resolve, reject) => {
    rpcPending.set(rpc, { resolve, reject });
    ctl({ rpc, method, args, container });
  });
}
function settleRpc(rpc, { result, error }) {
  const p = rpcPending.get(rpc);
  if (!p) return;
  rpcPending.delete(rpc);
  if (error != null) p.reject(new Error(error));
  else p.resolve(result ?? null);
}

let emitCounter = 0;
// Chunks a plugin asks oj to emit via `this.emitFile({ type: "chunk" })`.
// The reference id is minted here and returned synchronously (Rollup's emitFile
// is sync); the chunk is forwarded to rolldown as a build root by the Rust side,
// and its final hashed name is seeded back into the container's
// `chunkFileNames` before generateBundle so `this.getFileName(refId)` resolves.
const transformEmitStore = new AsyncLocalStorage();

// A minimal stand-in for Vite's `vite:css-post` plugin. UnoCSS (and similar)
// look up a plugin named "vite:css-post" in `config.plugins` during their build
// hook and call its `transform` with the CSS they generate, to route it into
// the output. oj collects that CSS here and folds it into the build stylesheet
// (see getPluginCss). Only relevant fields are implemented. Each container's
// resolved config lists its own, as each Vite environment resolves its own
// css-post: a worker bundle's CSS never joins the app's stylesheet.
const ojPluginCss = main.pluginCss;
const cssPostShimInto = (sink) => ({
  name: "vite:css-post",
  transform: {
    handler(code, id) {
      if (typeof code === "string" && code.length > 0) {
        sink.push({ id: typeof id === "string" ? id : "", css: code });
      }
      return null;
    },
  },
});
const cssPostShim = cssPostShimInto(ojPluginCss);

// A plugin container: the plugins one bundle runs and the state their hooks
// share (Vite's EnvironmentPluginContainer). The host's own environment is
// container 0 (`main`); a Start build gives each worker entry's bundle one of
// its own over `worker.plugins` (openWorkerContainer), as Vite's
// bundleWorkerEntry builds a BuildEnvironment, in the same realm as the app's
// plugins.
function createContainer(id, environment, resolvedConfig) {
  return {
    id,
    environment,
    resolvedConfig,
    // The plugins the container's hooks run (after applyToEnvironment), and
    // the set each environment resolves its own from.
    plugins: [],
    topLevelPlugins: [],
    emitted: [],
    chunkFileNames: new Map(),
    moduleInfoCache: new Map(),
    seenIds: new Set(),
    buildStartRun: null,
    buildStartFailureLogged: false,
    closeBundleDone: false,
    // The plugins as the config listed them, natively run ones included, and
    // what they routed through the container's css-post (getPluginCss).
    rawPlugins: [],
    pluginCss: [],
    // A config-phase hook of this container's plugins failed and was skipped:
    // plugins that share state then fail in each other's later hooks (Nitro's
    // buildApp after its config hook), so a buildApp failure is reported
    // instead of failing the build. A worker's own plugins never excuse the
    // app's.
    configHookSkipped: false,
    // Config functions a fact handed out (`$fn` handles), and the module
    // infos of the chunking pass in flight (callStartFnBatch).
    startFns: [],
    chunkInfos: { generation: null, byId: new Map() },
    oxcStripFilter: undefined,
    ctx: null,
    pluginCtxs: new WeakMap(),
    // Whether the framework plugins oj reimplements are left out (a Start
    // SSR or bundle container), and whether a native bundle drives the hooks.
    leaveOut: startLeavesOutReimplemented,
    bundle: ojBundleHost,
    // The config oj reads its own values from (facts, defines), when it
    // differs from the one the plugins see: a container that leaves plugins
    // out reads the config those plugins would not have shaped.
    ojConfig: null,
    configDelta: null,
    hookedConfig: null,
    // A container of the same environment as another (a Start bundle beside
    // the dev server's client): one buildStart for the environment.
    buildStartOwner: null,
    // A non-client dev environment: its buildStart waits for the client's
    // and runs only the plugins that ask for it (Vite's
    // perEnvironmentStartEndDuringDev).
    startAfter: null,
  };
}
const containers = new Map([[0, main]]);
let nextContainerId = 1;
function containerOf(id) {
  const c = containers.get(Number(id ?? 0));
  if (!c) throw new Error(`oj: plugin container ${id} is closed`);
  return c;
}

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

// this.parse is synchronous in Rollup, so resolve Vite's parseAst (re-exported
// from Rollup) once at startup; AST-aware plugins call it inside transform.
let viteParseAst = null;
// Vite's own `transformWithOxc`, what its `vite:oxc` plugin strips TS/JSX with.
let viteTransformWithOxc = null;
let viteCreateFilter = null;
// Vite's plugin context meta carries `viteVersion` (pluginContainer's
// basePluginContextMeta); plugins feature-detect on it. The app's own Vite
// reports its version; without one, the Vite line oj tracks.
let viteVersion = "8.0.0";
let rollupVersion = "4.0.0";
let rolldownVersion = "1.0.0";
try {
  const _root = initial.config?.root ?? process.cwd();
  const _vite = await import(appViteEntry(_root));
  if (typeof _vite.parseAst === "function") viteParseAst = _vite.parseAst;
  if (typeof _vite.transformWithOxc === "function") viteTransformWithOxc = _vite.transformWithOxc;
  if (typeof _vite.createFilter === "function") viteCreateFilter = _vite.createFilter;
  if (typeof _vite.version === "string") viteVersion = _vite.version;
  if (typeof _vite.rollupVersion === "string") rollupVersion = _vite.rollupVersion;
  if (typeof _vite.rolldownVersion === "string") rolldownVersion = _vite.rolldownVersion;
  if (!viteTransformWithOxc) {
    // An app Vite too old to export transformWithOxc: the vite:oxc-slot strip
    // cannot run, so normal/post plugins will see raw TS/JSX, exactly as
    // before the strip existed. Say so once instead of silently degrading.
    process.stderr.write(
      `oj plugin host: the app's vite (${viteVersion ?? "unknown"}) does not export transformWithOxc; ` +
        `normal/post transform plugins will receive raw TS/JSX.\n`,
    );
  }
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
function updateModuleInfo(c, id, result) {
  if (!result || typeof result !== "object") return;
  let info = c.moduleInfoCache.get(id);
  if (!info) c.moduleInfoCache.set(id, (info = makeModuleInfo(id, typeof result.code === "string" ? result.code : "")));
  if (result.meta && typeof result.meta === "object") info.meta = { ...(info.meta ?? {}), ...result.meta };
  if (result.moduleSideEffects != null) info.moduleSideEffects = result.moduleSideEffects;
  if (result.syntheticNamedExports != null) info.syntheticNamedExports = result.syntheticNamedExports;
}

// A container's plugin context, the `this` its hooks share (made on first use:
// its meta reads the app's Vite versions, resolved below).
function contextOf(c) {
  return (c.ctx ??= pluginContext(c));
}
function pluginContext(c) {
  return {
    environment: c.environment,
    meta: { viteVersion, rollupVersion, rolldownVersion, watchMode: command === "serve", framework: "oj" },
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
      if (source === "@vitejs/plugin-react/preamble" || source === "@vitejs/plugin-react-swc/preamble") {
        return { id: "/@oj/refresh-preamble.js" };
      }
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
        const prior = ambient
          ? ambient.findIndex((c) => c.id === source && c.importer === importer && c.plugin === self)
          : -1;
        if (prior !== -1) {
          skipCalls = ambient.slice();
          skipCalls[prior] = { ...skipCalls[prior], called: true };
        } else if (self) {
          skipCalls = ambient
            ? [...ambient, { id: source, importer, plugin: self }]
            : [{ id: source, importer, plugin: self }];
        }
      }
      const viaPlugin = await resolveIdFull(c, source, importer, {
        skipCalls,
        attributes: options && options.attributes,
        custom: options && options.custom,
        isEntry: !!(options && options.isEntry),
        scan: !!(this && this._scan),
      });
      if (viaPlugin != null) return viaPlugin;
      const map = transformResolveStore.getStore();
      if (map && map.has(source)) {
        const id = map.get(source);
        return id == null ? null : { id };
      }
      if (c.bundle) {
        const r = await asyncRpc("bundlerResolve", [source, importer ?? "", !!(options && options.isEntry)], c.id);
        // Outside a running bundle (no bundler to ask) oj's resolver answers.
        if (!r || !r.fallback) return r ?? null;
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
      c.emitted.push({ referenceId, fileName, source: String(file.source ?? "") });
      return referenceId;
    },
    getFileName(referenceId) {
      if (c.chunkFileNames.has(referenceId)) return c.chunkFileNames.get(referenceId);
      const f = c.emitted.find((e) => e.referenceId === referenceId);
      if (!f) throw new Error(`oj: unknown emit reference ${referenceId}`);
      return f.fileName;
    },
    async load(options) {
      const id = typeof options === "string" ? options : options.id;
      // Vite (pluginContainer ctx.load): container.load runs the plugins' load
      // chain, then transform, and hands back the module info. Only when no plugin
      // serves the id does the source come from disk (oj's native compile).
      let info = null;
      const fromPlugin = await loadFull(c, id);
      if (fromPlugin != null) {
        const outerWatch = transformWatchStore.getStore();
        updateModuleInfo(c, id, fromPlugin);
        const out = JSON.parse(await transform(c, fromPlugin.code, id, null));
        if (outerWatch) for (const f of out.watchFiles) outerWatch.add(f);
        info = c.moduleInfoCache.get(id) ?? makeModuleInfo(id, out.code);
        info.code = out.code;
      } else {
        const raw = await ctxRpc("moduleInfo", [id]);
        info = raw ? makeModuleInfo(raw.id ?? id, raw.code ?? "", raw) : null;
      }
      if (info) {
        c.moduleInfoCache.set(info.id, info);
        c.seenIds.add(info.id);
      }
      return info;
    },
    getModuleInfo(id) {
      const key = typeof id === "string" ? id : id.id;
      return bundleModuleInfo(c, key) ?? c.moduleInfoCache.get(key) ?? null;
    },
    addWatchFile(id) {
      if (!id) return;
      watchedFiles.add(String(id));
      const bucket = transformWatchStore.getStore();
      if (bucket) bucket.add(String(id));
    },
    getModuleIds() {
      const graph = bundleRpc("bundleModuleIds", [c.id]);
      return graph ? new Set([...graph, ...c.seenIds]).values() : c.seenIds.values();
    },
  };
}

// A container that runs inside a bundle (a Start bundle, `oj build`) reads
// the bundle's module graph, as Rollup's this.getModuleInfo does: rolldown
// has met every module it loads, a hook filter gate's skipped ones included,
// while the container's own records only hold the modules its hooks saw.
// Null while no bundle runs, or from a host without the engine bridge.
function bundleRpc(method, args) {
  if (!ojEngineRpc) return null;
  const raw = ojEngineRpc(method, JSON.stringify(args));
  return raw == null ? null : JSON.parse(raw);
}
// The bundle's ModuleInfo, on the container's own record of the module so
// the `meta` plugins set (and mutate) stays the one object Rollup keeps.
// `code` is read from the bundle when first asked for.
function bundleModuleInfo(c, id) {
  const graph = bundleRpc("bundleModuleInfo", [c.id, id]);
  if (!graph) return null;
  let info = c.moduleInfoCache.get(graph.id);
  if (!info) c.moduleInfoCache.set(graph.id, (info = makeModuleInfo(graph.id, null)));
  const { id: _, ...fields } = graph;
  Object.assign(info, fields);
  if (!Object.getOwnPropertyDescriptor(info, "code").get) {
    let code;
    Object.defineProperty(info, "code", {
      configurable: true,
      enumerable: true,
      get: () => (code ??= bundleRpc("bundleModuleCode", [c.id, info.id])),
      set: (v) => {
        code = v;
      },
    });
  }
  return info;
}

// Vite gives every plugin its own context object whose `_plugin` is the plugin
// itself (that is how ctx.resolve knows whom to skip). Derive a per-plugin view
// over the container's ctx so `this._plugin` is set inside the hooks.
function ctxFor(c, p) {
  const shared = contextOf(c);
  if (!p || typeof p !== "object") return shared;
  let own = c.pluginCtxs.get(p);
  if (!own) {
    own = Object.create(shared, { _plugin: { value: p } });
    c.pluginCtxs.set(p, own);
  }
  return own;
}

// Vite's ResolveIdContext carries the chain's skipCalls, so a nested
// ctx.resolve accumulates onto the whole chain instead of starting over, and
// the chain's `scan`, which a nested ctx.resolve passes on (`scan: this._scan`).
function resolveHookCtx(c, p, skipCalls, scan) {
  const base = ctxFor(c, p);
  if (!skipCalls && !scan) return base;
  return Object.create(base, { _resolveSkipCalls: { value: skipCalls }, _scan: { value: scan } });
}

// Vite's BasicMinimalPluginContext, the `this` of config, configEnvironment,
// configResolved and buildApp: the version meta (with `watchMode` for
// configResolved and buildApp), logging and error(), no environment.
function minimalContext(meta) {
  return {
    meta: { viteVersion, rollupVersion, rolldownVersion, ...meta },
    get pluginName() {
      return "";
    },
    debug() {},
    info(m) {
      process.stderr.write(pluginLogMessage("info", m, null));
    },
    warn(m) {
      process.stderr.write(pluginLogMessage("warning", m, null));
    },
    error(m) {
      throw typeof m === "string" ? new Error(m) : m;
    },
  };
}

let pluginConfigDelta = {};
// The user config after the plugins' config and configEnvironment hooks:
// Vite's `config` in resolveConfig, which a worker bundle's config starts from.
let hookedUserConfig = {};
// Vite's runConfigHook: every `config` hook of `list` in hook order, each
// handed the config so far and the ConfigEnv, what it returns merged in.
// `onPartial` sees each plugin's contribution; `keepPlugins` pins the plugin
// array across the merges (mergeConfigLite concatenates arrays). A skipped
// hook marks `container`.
async function runConfigHook(
  config,
  list,
  { onPartial, keepPlugins, container = main, recorded, replay, rebase = replay } = {},
) {
  const configContext = minimalContext();
  for (const { p, fn } of hooksOf(list, "config")) {
    initStage(`config:${p.name ?? "?"}`);
    try {
      let record = recorded?.get(p);
      // What a hook did inside the resolve the base came from (edits and the
      // partial both) is already in it; over any other base (no resolve, or
      // one that failed) it is made again, once.
      const inBase = !!record && !rebase;
      if (record) {
        if (rebase) applyEdits(config, record.edits);
        if (record.threw) throw record.threw;
      } else if (!replay) {
        const before = plainCopy(config);
        const partial = await fn.call(configContext, config, env);
        record = hookRecord(before, config, partial);
        if (recorded) recorded.set(p, record);
      }
      if (record?.edits.length) onPartial?.(editsPartial(record.edits));
      if (record?.partial) {
        // A copy: later hooks edit the merged config in place.
        if (!inBase) config = mergeConfigLite(config, plainCopy(record.partial));
        onPartial?.(record.partial);
      }
    } catch (e) {
      if (!ojStartMode) throw e;
      container.configHookSkipped = true;
      process.stderr.write(`${OJ} plugin host: config(${p.name ?? "?"}) skipped: ${(e && e.message) || e}\n`);
    }
    if (keepPlugins) config.plugins = keepPlugins;
  }
  return config;
}
// configResolved over `list` with `resolved`, on Vite's minimal context.
async function runConfigResolved(list, resolved, container = main) {
  const resolvedContext = minimalContext({ watchMode: command === "serve" });
  for (const { p, fn } of hooksOf(list, "configResolved")) {
    initStage(`configResolved:${p.name ?? "?"}`);
    try {
      await fn.call(resolvedContext, resolved);
    } catch (e) {
      if (!ojStartMode) throw e;
      container.configHookSkipped = true;
      process.stderr.write(`${OJ} plugin host: configResolved(${p.name ?? "?"}) skipped: ${(e && e.message) || e}\n`);
    }
  }
}
// Vite's resolveConfig over `rc` (the app's Vite resolve, or null) for the
// plugins of `list`: their config hooks, then every environment's
// configEnvironment, as `{ config, resolved, delta }` (the config before
// defaults, the resolved one, what the plugins contributed). With `replay`
// each hook answers what it returned the first time: a container leaving
// plugins out reads the config the rest made, and no hook runs twice.
async function hostConfig(rc, list, { replay = false, container = main } = {}) {
  // Whether the base is not a resolve the recorded hooks ran in (the app's,
  // or a replay of their answers).
  const rebase = !rc;
  // A copy: the hooks edit it in place, and every hostConfig starts from it.
  let config = plainCopy(initial.config ?? {});
  if (rc) {
    // The user's resolved config is the base; oj's own values (root, define,
    // server, environments) overlay it, but for the CLI-level ones Vite's
    // resolve already took in; the plugin list is replaced below.
    const { plugins: _fresh, ...base } = rc;
    config = mergeConfigLite(base, withoutCliInline(config));
  }
  // Coerce experimental.bundledDev off BEFORE the config/configResolved hooks
  // run, so TanStack's start-manifest-plugin emits the standard dev client entry
  // (not `/assets/index.js`) and any bundled-dev middleware early-returns.
  coerceBundledDevOff(config);
  // A config hook that ran inside Vite's resolve saw Vite's user config and
  // its plugin list; the hooks the host runs itself (no Vite in the app, or a
  // hook Vite's apply skipped) see this config, and configResolved sees it as
  // the resolved plugin array. Plugins like @crxjs read `config.plugins` to
  // find sibling plugins. Use the apply-filtered active set (not allPlugins)
  // so command-inappropriate plugins are absent, matching Vite's resolved
  // config — e.g. `@crxjs`'s serve-only `crx:hmr` must not appear during a
  // build, or crx calls its `transformCrxManifest` (which reads an unset
  // `config`) and throws. The exposed plugin array: the apply-filtered active
  // set plus the css-post shim. Pinned across the config-hook merges below so mergeConfigLite (which
  // concatenates arrays) can't accumulate it into duplicates.
  // The natively reimplemented React plugins stay listed (hooks unrun), in
  // enforce order like Vite's resolved plugin array.
  const configPlugins = withCssPostShim(list.concat(nativePlugins));
  config.plugins = configPlugins;
  let delta = {};
  config = await runConfigHook(config, list, {
    // What the plugins themselves contributed (Vite merges it into the
    // resolved config); Rust asks for it via getPluginConfig so values oj
    // applies natively (define) reach the compile too.
    onPartial: (partial) => (delta = mergeConfigLite(delta, partial)),
    // Keep the plugin array stable regardless of what a config hook returned.
    keepPlugins: configPlugins,
    // The hooks Vite's resolve already ran on these instances.
    recorded: recordedConfigPartials,
    replay,
    rebase,
    container,
  });
  // Vite runs `configEnvironment(name, options, env)` for every environment after
  // the config hooks and merges what it returns into that environment's options.
  // `env` is the one ConfigEnv plus isSsrTargetWebworker (config.ts
  // runConfigEnvironmentHook), the same for every environment.
  const envNames = Object.keys(config.environments ?? {});
  for (const n of ["client", "ssr"]) if (!envNames.includes(n)) envNames.push(n);
  const configEnvHooks = hooksOf(list, "configEnvironment");
  // Plugin by plugin, then environment by environment, as
  // runConfigEnvironmentHook loops.
  if (configEnvHooks.length) {
    config.environments = { ...(config.environments ?? {}) };
    for (const name of envNames) config.environments[name] = config.environments[name] ?? {};
    const opts = { ...env, isSsrTargetWebworker: false };
    const configContext = minimalContext();
    for (const { p, fn } of configEnvHooks) {
      initStage(`configEnvironment:${p.name ?? "?"}`);
      const seen = environmentPartials(p);
      for (const name of envNames) {
        try {
          let record = seen.get(name);
          const inBase = !!record && !rebase;
          if (record) {
            if (rebase) applyEdits(config.environments[name], record.edits);
            if (record.threw) throw record.threw;
          } else if (!replay) {
            const options = config.environments[name];
            const before = plainCopy(options);
            const r = await fn.call(configContext, name, options, opts);
            seen.set(name, (record = hookRecord(before, options, r)));
          }
          if (record?.partial && !inBase)
            config.environments[name] = mergeConfigLite(config.environments[name], plainCopy(record.partial));
        } catch (e) {
          if (!ojStartMode) throw e;
          container.configHookSkipped = true;
          process.stderr.write(
            `${OJ} plugin host: configEnvironment(${p.name ?? "?"}) skipped: ${(e && e.message) || e}\n`,
          );
        }
      }
    }
  }
  // A plugin's config hook can switch bundledDev back on (a dev plugin may do
  // so behind a flag); re-coerce so configResolved sees the truth oj serves, or
  // gated plugins run their bundled-dev-only setup.
  coerceBundledDevOff(config);
  const resolved = withResolvedDefaults(config);
  resolved.plugins = configPlugins;
  return { config, resolved, delta };
}
async function runConfigHooks() {
  userResolvedViteConfig = await loadUserResolvedViteConfig();
  initStage("user-config-resolved");
  const { config, resolved, delta } = await hostConfig(userResolvedViteConfig, main.plugins);
  pluginConfigDelta = delta;
  hookedUserConfig = config;
  main.resolvedConfig = resolved;
  const before = plainCopy(resolved);
  await runConfigResolved(main.plugins, main.resolvedConfig);
  configResolvedEdits = editsOf(before, main.resolvedConfig);
}
// What the configResolved hooks changed on the resolved config: Vite builds
// the environments after them, off the object they changed.
let configResolvedEdits = [];
// The config a container leaving the reimplemented framework plugins out
// reads its own values from, made once from the hooks' first answers.
let leftOutConfig = null;
function leaveOutConfig() {
  return (leftOutConfig ??= (async () => {
    const kept = (p) => !isReimplemented(p && p.name);
    const rc = await resolveUserViteConfig(allPlugins.filter(kept).map((p) => replayingPlugin(p, true)));
    return hostConfig(rc, main.topLevelPlugins.filter(kept), { replay: true, container: {} });
  })());
}
// The plugin array a resolved config lists: `list` in enforce order, with the
// vite:css-post shim when no plugin of that name is there.
function withCssPostShim(list, shim = cssPostShim) {
  const out = list.slice().sort((a, b) => enforceRank(a) - enforceRank(b));
  if (!out.some((p) => p && p.name === "vite:css-post")) out.push(shim);
  return out;
}
await runConfigHooks();
initStage("config-hooks-done");
// Vite's environment carries the resolved config (per-environment overrides
// merged), its logger and getTopLevelConfig(); applyToEnvironment and every
// hook's this.environment read them.
// A non-client environment re-resolves, so its `server.fs.allow` is a
// SNAPSHOT taken here (after runConfigHooks, so configResolved-time pushes
// are captured); Vite shares one `server` object across environments, so a
// post-boot push would be visible everywhere there. oj's serving allow-list
// reads the raw config on its own path and is unaffected either way.
// The plugins every environment resolves its own set from (the top-level
// resolved plugins, before applyToEnvironment).
main.topLevelPlugins = main.plugins;
async function resolveEnvironment(c, name) {
  const { environment, resolvedConfig } = c;
  environment.name = name;
  const own = (resolvedConfig.environments ?? {})[name] ?? {};
  environment.config = name === "client" ? resolvedConfig : withResolvedDefaults(mergeConfigLite(resolvedConfig, own));
  // The environment's own consumer: the client's config is the resolved
  // config itself, whose consumer every other environment would inherit.
  environment.config.consumer = own.consumer ?? (name === "client" ? "client" : "server");
  environment.logger = resolvedConfig.logger;
  environment.getTopLevelConfig = () => resolvedConfig;
  c.plugins = await environmentPlugins(environment, c.topLevelPlugins, c);
  environment.plugins = c.plugins;
}

// Vite (plugin.ts resolveEnvironmentPlugins): `applyToEnvironment` is awaited;
// a falsy result drops the plugin, `true` keeps it, and a plugin or plugin
// array replaces it with what it returned (perEnvironmentPlugin), whose
// config-phase hooks are ignored with a warning.
// Vite asks a plugin's applyToEnvironment once per environment (in
// resolveConfig); every container of that environment, and the real
// DevEnvironment of a runner-backed one, takes that one answer.
const appliedToEnvironments = new WeakMap();
function applyToEnvironmentOnce(p, environment) {
  let byName = appliedToEnvironments.get(p);
  if (!byName) appliedToEnvironments.set(p, (byName = new Map()));
  if (!byName.has(environment.name)) {
    byName.set(
      environment.name,
      (async () => {
        // A rejection nested in what it returned counts as its own throw.
        const applied = await p.applyToEnvironment(environment);
        return applied && applied !== true ? flattenPlugins(applied) : applied;
      })(),
    );
  }
  return byName.get(environment.name);
}
async function environmentPlugins(environment, candidates, c = main) {
  const ignoredEnvironmentPluginHooks = ["config", "configEnvironment", "configureServer", "configResolved"];
  const next = [];
  for (const p of candidates) {
    if (typeof p.applyToEnvironment !== "function") {
      next.push(p);
      continue;
    }
    let applied;
    initStage(`applyToEnvironment:${p.name ?? "?"}`);
    try {
      applied = await applyToEnvironmentOnce(p, environment);
    } catch (e) {
      // Vite's resolveEnvironmentPlugins lets it reject the resolve, and
      // with it createServer. Start mode tolerates a framework config's
      // lifecycle hooks per plugin, keeping the plugin active.
      if (!ojStartMode) throw decoratePluginError(e, p);
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
    const returned = applied.filter((ap) => !ojRunsNatively(ap, c));
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
  return next;
}
await resolveEnvironment(main, hostEnvName);
logActivePlugins(main.plugins);
function logActivePlugins(list) {
  process.stderr.write(
    `${OJ} plugin host: ${list.length} plugin(s) active for ${env.command}: ${list.map((p) => `${p.name}[${p.enforce ?? "-"}]`).join(",")}\n`,
  );
}

// Another environment on the host's plugin instances, as Vite's createServer
// gives every environment a container over the one resolved plugin array:
// the ssr environment's, or a Start bundle's beside the dev server's client.
// `leaveOut` drops the framework plugins oj reimplements (their state stays
// shared, their hooks unrun there); `bundle` ends `this.resolve` in the
// bundle's own resolution. A container of the host's own environment is that
// environment, not a second one: one environment object, one buildStart.
// Opened by id, so a revived engine reopens the same containers.
async function openEnvironmentContainer(spec) {
  const id = Number(spec.id);
  if (containers.has(id)) return id;
  const leaveOut = main.leaveOut || !!spec.leaveOut;
  const sameEnvironment = spec.name === main.environment.name;
  const environment = sameEnvironment ? main.environment : { name: spec.name, mode: main.environment.mode };
  const c = createContainer(id, environment, main.resolvedConfig);
  c.leaveOut = leaveOut;
  c.bundle = !!spec.bundle;
  c.durable = true;
  c.rawPlugins = main.rawPlugins;
  c.pluginCss = main.pluginCss;
  c.topLevelPlugins = leaveOut ? main.topLevelPlugins.filter((p) => !isReimplemented(p.name)) : main.topLevelPlugins;
  if (leaveOut && !main.leaveOut) {
    const view = await leaveOutConfig();
    c.ojConfig = view.resolved;
    c.configDelta = view.delta;
    c.hookedConfig = view.config;
  }
  if (sameEnvironment) {
    c.buildStartOwner = main;
    c.plugins = await environmentPlugins(environment, c.topLevelPlugins, c);
  } else {
    environment.transformRequest = transformRequestFor(c);
    environment.moduleGraph = createModuleGraph(c);
    environment.hot = serverHotChannel();
    await resolveEnvironment(c, spec.name);
    if (command === "serve") c.startAfter = main;
  }
  containers.set(id, c);
  logActivePlugins(c.plugins);
  return id;
}
for (const spec of ojEngineBoot?.containers ?? []) await openEnvironmentContainer(spec);

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
function knownModuleId(id, c = main) {
  if (c.seenIds.has(id)) return true;
  const file = String(id).split("?", 1)[0];
  if (!file || file.startsWith("\0") || !isAbsolute(file)) return false;
  try {
    return statSync(file).isFile();
  } catch {
    return false;
  }
}
// Vite graph nodes carry the module's served URL beside its resolved id: HMR
// update frames are built from `node.url`, and the client matches them against
// the URLs it imported. A resolved id that names a plain app file maps to the
// URL oj serves it at (root-relative, or /@fs for an outside-root file);
// everything else (virtuals, queries, dependencies) keeps the id, as before.
function applicationModuleUrl(id) {
  const file = String(id);
  if (!isAbsolute(file) || file.includes("?") || file.split(/[\\/]/).includes("node_modules")) return id;
  try {
    if (!statSync(file).isFile()) return id;
  } catch {
    return id;
  }
  const root = main.resolvedConfig?.root ?? initial.config?.root ?? process.cwd();
  const rel = relative(root, file);
  return rel === ".." || rel.startsWith("../") || rel.startsWith("..\\") || isAbsolute(rel)
    ? `/@fs${file.replaceAll("\\", "/")}`
    : `/${rel.replaceAll("\\", "/")}`;
}
// One per environment, as each Vite DevEnvironment has its own: `c` is the
// container whose hooks it knows ids from.
function createModuleGraph(c = main) {
  const graphNodes = new Map();
  const urlNodes = new Map();
  const fileToModulesMap = new Map();
  function moduleNode(id) {
    let n = graphNodes.get(id);
    if (!n) {
      const file = String(id).split("?", 1)[0];
      n = {
        id,
        url: applicationModuleUrl(id),
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
      urlNodes.set(n.url, n);
      let byFile = fileToModulesMap.get(file);
      if (!byFile) fileToModulesMap.set(file, (byFile = new Set()));
      byFile.add(n);
    }
    return n;
  }
  return {
    getModuleById: (id) => (id == null || !knownModuleId(String(id), c) ? undefined : moduleNode(String(id))),
    // By the served URL first (a node's own `url` round-trips, as in Vite),
    // falling back to the id-shaped lookup plugins used before.
    getModuleByUrl: async (url) =>
      url == null
        ? undefined
        : (urlNodes.get(String(url)) ?? (knownModuleId(String(url), c) ? moduleNode(String(url)) : undefined)),
    getModulesByFile: (file) =>
      fileToModulesMap.get(String(file)) ??
      (knownModuleId(String(file), c) ? new Set([moduleNode(String(file))]) : undefined),
    // Vite creates the node on demand here (used by plugins that pre-seed urls).
    ensureEntryFromUrl: async (url) => moduleNode(String(url)),
    invalidateModule(mod) {
      if (!mod) return;
      mod.lastInvalidationTimestamp = Date.now();
      mod.transformResult = null;
      mod.ssrTransformResult = null;
      ojServerEvent("invalidate", { id: mod.id, environment: c.environment.name });
    },
    invalidateAll() {
      for (const m of graphNodes.values()) m.transformResult = null;
      ojServerEvent("invalidateAll", { environment: c.environment.name });
    },
    onFileChange(file) {
      ojServerEvent("invalidate", { id: String(file), environment: c.environment.name });
    },
    urlToModuleMap: urlNodes,
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
// A server environment's hot channel (Vite's createServerHotChannel): its
// messages are its module runner's, never the browser's, and oj's runners
// take none, so a send reaches only what listens on the channel itself.
function serverHotChannel() {
  const listeners = new Map();
  return {
    on(event, cb) {
      let a = listeners.get(event);
      if (!a) listeners.set(event, (a = new Set()));
      a.add(cb);
    },
    off(event, cb) {
      listeners.get(event)?.delete(cb);
    },
    send() {},
    listen() {},
    close() {},
  };
}

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
// Vite emits the http server's "listening" only when the socket is really
// bound (createServer runs configureServer, listen() comes after). In-process,
// oj binds AFTER this host boots, and announces the bound port with a
// `serverListening` hook call; the stub emits then, so a plugin's
// once("listening") fetch of its own server connects instead of being refused.
// Under plain node (the direct-drive unit tests) no announce exists and the
// emit stays at configureServer end, as before.
let ojDevListenerBound = false;
let ojDevListenerPort = null;
let ojDevListenerAddress = null;
let ojConfigureServerDone = false;
async function ojAnnounceDevListener(port, address) {
  ojDevListenerBound = true;
  if (Number.isFinite(port) && port > 0) ojDevListenerPort = port;
  if (typeof address === "string" && address) ojDevListenerAddress = address;
  const httpServer = devServer && devServer.httpServer;
  if (!ojConfigureServerDone || !httpServer || httpServer.listening) return;
  await ojEmitListening(devServer);
  // A listening callback may have registered middleware or upgrade listeners
  // that the setup-time check saw none of; give them the forwarding server.
  await ensureConfigureServerMiddleware();
}

// Vite sets `server.resolvedUrls` in a PREPENDED "listening" handler
// (server/index.ts listen()), so every plugin's own listening callback can
// already read it; mirror that ordering by resolving right before the emit.
// Vite also runs the client buildStart before the socket listens (initServer
// inside the wrapped httpServer.listen), so under Vite "listening" implies
// buildStart settled — gate the in-process emit the same way. oj's Rust
// listener serves regardless, so a failed buildStart logs (inside the gate)
// and the emit still fires instead of stranding listening waiters, the same
// degrade-not-die stance the gated serving hooks take.
async function ojEmitListening(server) {
  if (ojEnginePost) await gateOnBuildStart(main);
  // A second announce (bind and ignite both deliver on close races) can pass
  // the caller's `listening` check while this one is parked on the gate;
  // node fires "listening" once per listen, so re-check past the await.
  if (server.httpServer.listening) return;
  try {
    server.resolvedUrls = ojResolveServerUrls();
  } catch (e) {
    // A null resolvedUrls at listening breaks Vite's guarantee: say why.
    process.stderr.write(`${OJ} plugin host: resolvedUrls failed: ${(e && e.message) || e}\n`);
  }
  server.httpServer.emit("listening");
}

// Vite's constants.ts loopbackHosts / wildcardHosts.
const OJ_LOOPBACK_HOSTS = new Set(["localhost", "127.0.0.1", "::1", "0000:0000:0000:0000:0000:0000:0000:0001"]);
const OJ_WILDCARD_HOSTS = new Set(["0.0.0.0", "::", "0000:0000:0000:0000:0000:0000:0000:0000"]);

// Vite's utils.ts resolveHostname, minus the async localhost-vs-DNS probe
// (it only reorders the printed name on machines where the two disagree; the
// terminal URLs are oj's own, printed by Rust).
function ojResolveHostname(optionsHost) {
  let host;
  if (optionsHost === undefined || optionsHost === null || optionsHost === false) host = "localhost";
  else if (optionsHost === true) host = undefined;
  else host = optionsHost;
  const name = host === undefined || OJ_WILDCARD_HOSTS.has(host) ? "localhost" : host;
  return { host, name };
}

// Vite's utils.ts resolveServerUrls over the announced (or configured) port,
// minus the https-cert hostname extraction (the stub carries no httpsOptions).
function ojResolveServerUrls() {
  const cfg = main.resolvedConfig ?? initial.config ?? {};
  const serverCfg = cfg.server ?? {};
  const port = ojDevListenerPort ?? serverCfg.port;
  if (!port) return { local: [], network: [], networkInterfaceNames: [] };
  const hostname = ojResolveHostname(serverCfg.host);
  const protocol = serverCfg.https ? "https" : "http";
  const rawBase = cfg.rawBase ?? cfg.base;
  const base = rawBase == null || rawBase === "./" || rawBase === "" ? "/" : rawBase;
  const local = [];
  const network = [];
  const networkInterfaceNames = [];
  if (hostname.host !== undefined && !OJ_WILDCARD_HOSTS.has(hostname.host)) {
    let name = hostname.name;
    if (name.includes(":")) name = `[${name}]`;
    const url = `${protocol}://${name}:${port}${base}`;
    if (OJ_LOOPBACK_HOSTS.has(hostname.host)) {
      local.push(url);
    } else {
      network.push(url);
      let interfaceName;
      for (const [ifName, nInterface] of Object.entries(os.networkInterfaces())) {
        if ((nInterface ?? []).some((detail) => detail.address === hostname.host)) {
          interfaceName = ifName;
          break;
        }
      }
      networkInterfaceNames.push(interfaceName);
    }
  } else {
    for (const [ifName, nInterface] of Object.entries(os.networkInterfaces())) {
      for (const detail of nInterface ?? []) {
        if (!detail.address || detail.family !== "IPv4") continue;
        let hostPart = detail.address.replace("127.0.0.1", hostname.name);
        if (hostPart.includes(":")) hostPart = `[${hostPart}]`;
        const url = `${protocol}://${hostPart}:${port}${base}`;
        if (detail.address.includes("127.0.0.1")) {
          local.push(url);
        } else {
          network.push(url);
          networkInterfaceNames.push(ifName);
        }
      }
    }
  }
  return { local, network, networkInterfaceNames };
}
let appVite = null;
async function loadAppVite() {
  if (appVite) return appVite;
  try {
    const root =
      (main.resolvedConfig && main.resolvedConfig.root) || (initial.config && initial.config.root) || process.cwd();
    appVite = await import(appViteEntry(root));
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
  const root =
    (main.resolvedConfig && main.resolvedConfig.root) || (initial.config && initial.config.root) || process.cwd();
  // Real DevEnvironments need a real resolved Vite config (root, cacheDir,
  // resolve.builtins, per-env dev.createEnvironment factories): the app's
  // Vite resolved it once already, over these very plugin instances, for the
  // config the host itself runs on. The environments are built from that
  // resolve, as Vite's createServer builds them from its one resolveConfig:
  // no second evaluation, no hook run twice on an instance (a second config
  // hook corrupts stateful plugins, @cloudflare/vite-plugin's
  // virtual:cloudflare/export-types among them).
  let rc;
  try {
    hostPhase("cf: environments config begin");
    initStage("cf:environments-config");
    rc = await environmentsConfig();
    if (!rc) return undefined;
    hostPhase("cf: environments config done");
    initStage("cf:environments-config-done");
    // Coerce bundledDev off BEFORE the environments are built off `rc`: the
    // DevEnvironment constructor makes the client env bundled from these
    // fields, and the manifest plugin bound to these environments reads them
    // at load time.
    coerceBundledDevOff(rc);
    // The boot-time deps pre-seed runs alongside this host's start: the
    // snapshot below and the deps caches the environments load are its output.
    await bootPreseedDone();
    // BEFORE the environments are built: include must be in the config the
    // optimizer hashes, or the preseed child's metadata cannot validate.
    foldIncludeSnapshot(rc, initial.preseedIncludePath);
  } catch (e) {
    process.stderr.write(`${OJ} plugin host: the environments' config failed: ${(e && e.message) || e}\n`);
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
  await Promise.all(
    Object.entries(rc.environments || {}).map(async ([name, envOpts]) => {
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
    }),
  );
  deferBootRegistrations(environments);
  return environments;
}

// The deps a server environment's optimizer is handed while the server sets
// up (a plugin's configureServer registering the polyfills its runtime
// injects, @cloudflare/vite-plugin's nodejs_compat ones among them) are part
// of the boot-time pass the deps pre-seed child runs out of process: Vite's
// optimizer would rerun for them in this process, and rolldown keeps the
// native memory of every build until the process exits. So until setup ends
// they are collected, optimized by a one-shot child with the same config and
// the same registrations, and the environment's optimizer takes the child's
// committed metadata. A registration after setup is the session discovering
// a dep, which the optimizer handles here as Vite does.
let bootRegistrationsOpen = false;
const bootRegistrationBatches = [];
function deferBootRegistrations(environments) {
  if (!ojEnginePost) return;
  bootRegistrationsOpen = true;
  let batch = null;
  for (const [name, environment] of Object.entries(environments)) {
    const optimizer = environment?.depsOptimizer;
    if (!optimizer || typeof optimizer.registerMissingImport !== "function") continue;
    const register = optimizer.registerMissingImport;
    optimizer.registerMissingImport = (id, resolved) => {
      const metadata = optimizer.metadata;
      const known = metadata?.optimized?.[id] ?? metadata?.chunks?.[id] ?? metadata?.discovered?.[id];
      if (known || !bootRegistrationsOpen) return known ?? register(id, resolved);
      if (!batch) {
        batch = { deps: {}, done: null };
        const current = batch;
        // One child at a time: each commits over the last one's metadata.
        const previous = bootRegistrationBatches.at(-1) ?? Promise.resolve();
        current.done = new Promise((resolve) => setTimeout(resolve, 50))
          .then(() => {
            batch = null;
            return previous;
          })
          .then(() => seedBootRegistrations(environments, current.deps))
          // A failed batch is logged; it never holds the ones after it.
          .catch((e) => process.stderr.write(`${OJ} plugin host: boot deps batch failed: ${(e && e.message) || e}\n`));
        bootRegistrationBatches.push(current.done);
      }
      const deps = (batch.deps[name] ??= {});
      if (!deps[id]) {
        // A discovered dep as Vite's addMissingDep makes it: what an import
        // of it resolves to meanwhile, its loads waiting out `processing`.
        const info = {
          id,
          file: normalizePath(pathResolve(depsCacheDir(environment), `${flattenDepId(id)}.js`)),
          src: resolved,
          browserHash: discoveredBrowserHash(metadata),
          // What took its place: the child's dep, or this process's
          // optimizer's. Never itself (a batch that failed left it there).
          processing: batch.done.then(() => {
            const now = optimizer.metadata;
            const next = now.optimized[id] ?? now.discovered[id];
            return next && next !== info ? next.processing : undefined;
          }),
        };
        metadata.discovered[id] = info;
        metadata.depInfoList.push(info);
        deps[id] = info;
      }
      return deps[id];
    };
    optimizer.registerMissingImport.ojRegister = register;
  }
}
// Vite's getDiscoveredBrowserHash, over this session's start: a new
// discovery set gets a new hash, so the browser never reuses a stale dep.
const discoverySession = String(Date.now());
function discoveredBrowserHash(metadata) {
  const srcs = (infos) => Object.fromEntries(Object.entries(infos ?? {}).map(([k, v]) => [k, v.src]));
  return createHash("sha256")
    .update(
      metadata.hash +
        JSON.stringify(srcs(metadata.optimized)) +
        JSON.stringify(srcs(metadata.discovered)) +
        discoverySession,
    )
    .digest("hex")
    .substring(0, 8);
}
// Until the dev server's boot-time deps pre-seed is over (preseed.rs), polled
// over the synchronous bridge: the host is still in its top-level init, so a
// milestone each second keeps the stall monitor from reading the wait as a
// wedge (the pre-seed's own kill bounds it).
async function bootPreseedDone() {
  hostPhase("waiting for the deps pre-seed");
  try {
    for (let polls = 0; bundleRpc("preseedBootDone", []) === false; polls++) {
      if (polls % 20 === 0) initStage("deps-pre-seed");
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
  } catch (e) {
    process.stderr.write(`${OJ} plugin host: deps pre-seed wait failed: ${(e && e.message) || e}\n`);
  }
  hostPhase("deps pre-seed over");
}
// One batch of setup-time registrations: the child optimizes them, then each
// optimizer reads the metadata it committed; a failed child leaves them to
// this process's optimizer, as without the pre-seed.
async function seedBootRegistrations(environments, deps) {
  const request = {};
  for (const [name, byId] of Object.entries(deps)) request[name] = Object.values(byId).map((d) => [d.id, d.src]);
  // Over the synchronous bridge, polled: the setup may still be running,
  // waiting on these very deps, before any hook reply could reach the host.
  let seeded = false;
  try {
    const run = bundleRpc("preseedRegistered", [env.mode, JSON.stringify(request)]);
    while (run != null) {
      const done = bundleRpc("preseedRegisteredDone", [run]);
      if (done != null) {
        seeded = done === true;
        break;
      }
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
  } catch (e) {
    process.stderr.write(`${OJ} plugin host: boot deps pre-optimization failed: ${(e && e.message) || e}\n`);
  }
  for (const [name, byId] of Object.entries(deps)) {
    const optimizer = environments[name]?.depsOptimizer;
    if (!optimizer) continue;
    try {
      if (seeded && reloadOptimizerMetadata(environments[name], optimizer, Object.keys(byId))) continue;
    } catch (e) {
      process.stderr.write(`${OJ} plugin host: the pre-seeded deps metadata is unreadable: ${(e && e.message) || e}\n`);
    }
    // Left to this process's optimizer: the stand-ins make way for its own.
    const metadata = optimizer.metadata;
    const standIns = new Set(Object.values(byId));
    for (const id of Object.keys(byId)) if (standIns.has(metadata.discovered[id])) delete metadata.discovered[id];
    metadata.depInfoList = metadata.depInfoList.filter((info) => !standIns.has(info));
    for (const dep of standIns) {
      try {
        optimizer.registerMissingImport.ojRegister(dep.id, dep.src);
      } catch (e) {
        process.stderr.write(`${OJ} plugin host: optimizing ${dep.id} failed: ${(e && e.message) || e}\n`);
      }
    }
  }
}
// The optimizer takes what the child committed as Vite's rerun commits its
// own result (createDepsOptimizer runOptimizer): unchanged deps keep the
// browser hash their importers already use, the discovered stand-ins learn
// their final shape, and a change to a dep already served reloads the
// environment. In place: the optimizer reads that one metadata object.
// False when the committed metadata is missing one of `ids`.
function reloadOptimizerMetadata(environment, optimizer, ids) {
  const dir = depsCacheDir(environment);
  let raw;
  try {
    raw = readFileSync(join(dir, "_metadata.json"), "utf8");
  } catch {
    return false;
  }
  // Vite's parseDepsOptimizerMetadata.
  const parsed = JSON.parse(raw, (key, value) =>
    key === "file" || key === "src" ? normalizePath(pathResolve(dir, value)) : value,
  );
  if (!parsed.chunks || !parsed.optimized || ids.some((id) => !parsed.optimized[id])) return false;
  const next = { optimized: {}, chunks: {}, discovered: {}, depInfoList: [] };
  const add = (type, info) => {
    next[type][info.id] = info;
    next.depInfoList.push(info);
  };
  for (const [id, info] of Object.entries(parsed.optimized))
    add("optimized", { ...info, id, browserHash: parsed.browserHash });
  for (const [id, info] of Object.entries(parsed.chunks)) {
    add("chunks", { ...info, id, browserHash: parsed.browserHash, needsInterop: false });
  }
  const metadata = optimizer.metadata;
  const interopMismatch = Object.entries(metadata.discovered).some(
    ([id, info]) =>
      info.needsInterop !== undefined && next.optimized[id] && next.optimized[id].needsInterop !== info.needsInterop,
  );
  const needsReload =
    interopMismatch ||
    metadata.hash !== parsed.hash ||
    Object.keys(metadata.optimized).some((id) => metadata.optimized[id].fileHash !== next.optimized[id]?.fileHash);
  for (const [id, info] of Object.entries(metadata.discovered)) if (!next.optimized[id]) add("discovered", info);
  let browserHash = parsed.browserHash;
  if (!needsReload) {
    browserHash = metadata.browserHash;
    for (const info of Object.values(next.chunks)) info.browserHash = browserHash;
    for (const [id, info] of Object.entries(next.optimized)) {
      info.browserHash = (metadata.optimized[id] || metadata.discovered[id])?.browserHash ?? browserHash;
    }
  }
  for (const [id, optimized] of Object.entries(next.optimized)) {
    const discovered = metadata.discovered[id];
    if (!discovered) continue;
    discovered.browserHash = optimized.browserHash;
    discovered.fileHash = optimized.fileHash;
    discovered.needsInterop = optimized.needsInterop;
    discovered.processing = undefined;
  }
  Object.assign(metadata, {
    hash: parsed.hash,
    lockfileHash: parsed.lockfileHash,
    configHash: parsed.configHash,
    browserHash,
    ...next,
  });
  if (needsReload) {
    environment.logger?.info?.("optimized dependencies changed. reloading", { timestamp: true });
    environment.moduleGraph?.invalidateAll?.();
    environment.hot?.send?.({ type: "full-reload", path: "*" });
  }
  return true;
}
// Vite's normalizePath, getDepsCacheDir and flattenId.
function normalizePath(p) {
  return p.replace(/\\/g, "/");
}
function depsCacheDir(environment) {
  const { cacheDir } = environment.config;
  return normalizePath(pathResolve(cacheDir, environment.name === "client" ? "deps" : `deps_${environment.name}`));
}
function flattenDepId(id) {
  const flat = id
    .replaceAll(/_+/g, "$&__")
    .replaceAll("/", "_")
    .replaceAll(".", "__")
    .replace(/\s*>\s*/g, "_n_")
    .replace(/[\u0000-\u001F"#$%&*+,:;<=>?[\]^`{|}\u007F]/g, (c) => "_0" + c.charCodeAt(0).toString(16) + "_");
  return flat.length > 170
    ? `${flat.slice(0, 161)}_${createHash("sha256").update(flat).digest("hex").slice(0, 8)}`
    : flat;
}

// The resolve the host's config came from, as Vite's resolveConfig leaves
// it: its plugin array the host's instances (not the resolve's recording
// stand-ins), each environment's plugins what applyToEnvironment kept of them.
async function environmentsConfig() {
  const base = userResolvedViteConfig ?? (await replayedViteConfig());
  if (!base) return null;
  applyEdits(base, configResolvedEdits);
  configResolvedEdits = [];
  const plugins = (base.plugins ?? []).map((p) => standInFor.get(p) ?? p);
  const rc = { ...base, plugins, ...pluginHookUtils(plugins), environments: {} };
  // The host resolved silently; the environments log as the app's Vite would.
  const vite = await loadAppVite();
  if (typeof vite?.createLogger === "function") {
    rc.logLevel = userViteConfig?.logLevel ?? "info";
    rc.logger = vite.createLogger(rc.logLevel, {
      allowClearScreen: false,
      customLogger: userViteConfig?.customLogger,
    });
  }
  for (const [name, options] of Object.entries(base.environments ?? {})) {
    rc.environments[name] = { ...options, dev: { ...options?.dev }, optimizeDeps: { ...options?.optimizeDeps } };
  }
  for (const name of Object.keys(rc.environments)) {
    rc.environments[name].plugins = await viteEnvironmentPlugins(name, rc);
  }
  return rc;
}
// The app's Vite resolving the host's own plugins (an oj plugins file has no
// vite.config to resolve), each hook answering what it returned in the host.
async function replayedViteConfig() {
  const vite = await loadAppVite();
  if (!vite || typeof vite.resolveConfig !== "function") return null;
  const root = initial.config?.root ?? process.cwd();
  try {
    unsetOjNodeEnvForResolve();
    return await asNode(() =>
      vite.resolveConfig(
        {
          root,
          configFile: false,
          plugins: allPlugins.map((p) => replayingPlugin(p, true)),
          mode: env.mode,
          logLevel: "silent",
        },
        env.command,
        env.mode,
        defaultNodeEnv,
      ),
    );
  } finally {
    if (!process.env.NODE_ENV) process.env.NODE_ENV = defaultNodeEnv;
  }
}
// Vite's resolveEnvironmentPlugins over a PartialEnvironment of `rc`.
async function viteEnvironmentPlugins(name, rc) {
  const options = rc.environments[name];
  const environment = {
    name,
    config: new Proxy(options, { get: (target, key) => (key in target ? target[key] : rc[key]) }),
    getTopLevelConfig: () => rc,
    logger: rc.logger,
  };
  const out = [];
  for (const p of rc.plugins) {
    if (!p) continue;
    if (typeof p.applyToEnvironment !== "function") {
      out.push(p);
      continue;
    }
    const applied = await applyToEnvironmentOnce(p, environment);
    if (!applied) continue;
    if (applied === true) out.push(p);
    else out.push(...applied);
  }
  return out;
}
// Vite's warmupFiles for the runner environments: each environment's own
// `dev.warmup` (`server.warmup.ssrFiles` for ssr), fire-and-forget as in
// Vite (transformRequest dedups, warmupRequest swallows outdated-dep and
// closed-server errors). The reply reports what STARTED. The client
// environment is never warmed: oj's own pipeline serves the browser, and the
// in-host client environment's module graph is read by nothing.
async function warmEnvironments() {
  const envs = devServer?.environments ?? {};
  const config = devServer?.config ?? main.resolvedConfig ?? {};
  const root = config.root ?? initial.config?.root ?? process.cwd();
  const names = [];
  let started = 0;
  for (const [name, environment] of Object.entries(envs)) {
    if (typeof environment?.warmupRequest !== "function") continue;
    const options = config.environments?.[name] ?? {};
    if ((options.consumer ?? (name === "client" ? "client" : "server")) === "client") continue;
    const patterns = environment.config?.dev?.warmup ?? options.dev?.warmup ?? [];
    const files = await warmupFiles(Array.isArray(patterns) ? patterns : [], root);
    if (!files.length) continue;
    names.push(name);
    for (const file of files) {
      started++;
      const rel = relative(root, file).replace(/\\/g, "/");
      const url = rel.startsWith("..") || isAbsolute(rel) ? `/@fs/${file.replace(/^\//, "")}` : `/${rel}`;
      // A custom environment's synchronously throwing warmupRequest must not
      // escape the dispatch (Vite's own never rejects).
      void (async () => environment.warmupRequest(url))().catch(() => {});
    }
  }
  return names.length ? JSON.stringify({ environments: names, started }) : null;
}
// Vite's mapFiles: plain entries resolved from the root, globs expanded.
async function warmupFiles(files, root) {
  const { tinyglobby } = viteGlobs();
  const out = [];
  const globs = [];
  for (const file of files) {
    if (typeof file !== "string") continue;
    if (tinyglobby.isDynamicPattern(file)) globs.push(file);
    else out.push(isAbsolute(file) ? file : pathResolve(root, file));
  }
  if (globs.length) {
    out.push(
      ...(await tinyglobby.glob(globs, {
        absolute: true,
        cwd: root,
        expandDirectories: false,
        ignore: ["**/.git/**", "**/node_modules/**"],
      })),
    );
  }
  return out;
}

// Vite's createPluginHookUtils: a hook's plugins by its `order`.
function pluginHookUtils(plugins) {
  const sorted = new Map();
  const getSortedPlugins = (hook) => {
    if (!sorted.has(hook)) {
      const rank = (p) => hookOrderRank(p[hook]);
      sorted.set(
        hook,
        plugins.filter((p) => p && p[hook]).sort((a, b) => rank(a) - rank(b)),
      );
    }
    return sorted.get(hook);
  };
  const getSortedPluginHooks = (hook) =>
    getSortedPlugins(hook)
      .map((p) => hookHandler(p[hook]))
      .filter(Boolean);
  return { getSortedPlugins, getSortedPluginHooks };
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
// writes all hash differently and process. Only a file some runner graph
// knows is read: it is a module, and its update depends on its bytes. Any
// other file is identified by its stat and never read, as Vite never reads a
// changed file unless a plugin asks (`ctx.read()`); hashing a database a
// worker keeps writing (miniflare's trace store grows to gigabytes) read it
// whole on every write and blocked the host. A stat or read error means the
// file's state is unknown: process and forget the entry, never suppress on
// error. Consecutive deletes dedup by type alone.
const lastProcessedChange = new Map(); // file -> { type, contentHash }
// Correctness never depends on an entry existing (a cleared entry only costs
// one extra processing pass), so bound the map instead of growing forever.
const LAST_PROCESSED_MAX = 5000;
function freshChange({ file, real, type }, environments) {
  let contentHash = null;
  if (type !== "delete") {
    try {
      if (graphKnowsFile(environments, file) || (real && graphKnowsFile(environments, real))) {
        contentHash = createHash("sha256").update(readFileSync(file)).digest("hex");
      } else {
        // Nanosecond times: a database rewrites pages in place at a fixed
        // size, so two writes inside one millisecond must still differ.
        const st = statSync(file, { bigint: true });
        contentHash = `stat:${st.ino}:${st.size}:${st.mtimeNs}:${st.ctimeNs}`;
      }
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
// It also decides which changed files freshChange reads to hash.
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
// Vite keys a module's `file` by its canonical path (its resolver realpaths
// every file it resolves unless `resolve.preserveSymlinks` is set), while a
// watcher reports the spelling it watched: a file reached through a symlink
// (a linked workspace package, a symlinked app dir) diverges, matches no
// module, and the runner keeps serving the old copy. The real path is the
// second spelling every lookup tries.
function realSpelling(file) {
  try {
    const real = realpathSync(file).replace(/\\/g, "/");
    return real === file ? null : real;
  } catch {
    return null;
  }
}
// Vite handles each watcher event on its own, so a burst's changes run their
// hooks concurrently: a hook that debounces by awaiting a timer and checking
// for a newer call (a ticket) sees that newer call. Here each file has its own
// chain: a file's changes run in arrival order (the early and settled sends
// for one edit never interleave their graph walks), other files never wait.
const fileTails = new Map(); // file -> its latest change, settled or not
// A resync runs after every change queued before it, and every change queued
// after it waits for it.
let resyncBarrier = Promise.resolve();
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
    try {
      env.moduleGraph?.invalidateAll?.();
    } catch {}
    hotSend(env, { type: "full-reload", path: "*" });
  }
}
function enqueueResync(run) {
  const queued = [resyncBarrier, ...fileTails.values()];
  resyncBarrier = Promise.allSettled(queued).then(run);
  return resyncBarrier;
}
// Resolves to how many changes matched a runner-backed graph, or null when a
// change's handling threw (the caller must assume a hit).
async function invalidateEnvironments(environments, watcher, changes) {
  const runs = changes.map(({ path, type }) => {
    const file = String(path).replace(/\\/g, "/");
    const run = Promise.allSettled([resyncBarrier, fileTails.get(file)])
      .then(() => invalidateChange(environments, watcher, file, type || "update"))
      .catch(() => null);
    fileTails.set(file, run);
    run.then(() => {
      if (fileTails.get(file) === run) fileTails.delete(file);
    });
    return run;
  });
  const results = await Promise.all(runs);
  if (results.includes(null)) return null;
  return results.filter(Boolean).length;
}
// One change, as Vite's watcher handles one event: watcher listeners, then
// each runner-backed environment's hotUpdate pipeline. Returns whether a
// graph matched it.
async function invalidateChange(environments, watcher, file, type) {
  // Re-classify at handling time: the early (pre-settle) send classifies from
  // a raw event, and an atomic-save editor's momentary delete may have been
  // recreated by the time this runs; a stale "delete" would prune a live
  // module from the graph.
  const real = realSpelling(file);
  if (type === "delete" && existsSync(file)) type = "update";
  if (type === "create" && (graphKnowsFile(environments, file) || (real && graphKnowsFile(environments, real))))
    type = "update";
  if (!freshChange({ file, real, type }, environments)) return false;
  try {
    watcher.emit(WATCHER_EVENT[type], file);
  } catch {}
  if (!environments) return false;
  const timestamp = Date.now();
  let matched = false;
  for (const env of Object.values(environments)) {
    // oj's own HMR already covers the stub environments; only runner-backed
    // (real Vite) environments need their graph invalidated and updates sent.
    // The client environment is oj's own: the dev server's pass runs its
    // hooks once (Vite's single pass per environment).
    if (!env || env.__ojStub || env.name === "client") continue;
    try {
      let hit = await hotUpdateEnvironment(env, file, timestamp, type);
      if (!hit && real) hit = await hotUpdateEnvironment(env, real, timestamp, type);
      if (hit) matched = true;
    } catch (e) {
      process.stderr.write(`${OJ} plugin host: hot update failed (${env.name}): ${(e && e.stack) || e}\n`);
      hotSend(env, { type: "error", err: prepareErrorPayload(e) });
    }
  }
  // A changed file no runner-backed graph knows is Vite's "no modules matched"
  // (nothing is sent), but here it can also mean the watcher path spells the
  // file differently than the graph keys (casing; the symlink case is retried
  // through its real path above) — make the staleness visible once per file
  // instead of silently serving old modules.
  if (type !== "delete" && !matched && !unmatchedChangeLogged.has(file)) {
    unmatchedChangeLogged.add(file);
    process.stderr.write(`${OJ} plugin host: change to ${file} matched no module in any runner environment\n`);
  }
  return matched;
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
  let pre = 0,
    normal = 0,
    post = 0;
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
    updates.push(
      ...boundaries.map(({ boundary, acceptedVia, isWithinCircularImport }) => ({
        type: `${boundary.type ?? "js"}-update`,
        timestamp,
        path: normalizeHmrUrl(boundary.url),
        acceptedPath: normalizeHmrUrl(acceptedVia.url),
        explicitImportRequired: (boundary.type ?? "js") === "js" ? isExplicitImportRequired(acceptedVia.url) : false,
        isWithinCircularImport,
      })),
    );
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
    boundaries.push({
      boundary: node,
      acceptedVia: node,
      isWithinCircularImport: isNodeWithinCircularImports(node, currentChain),
    });
    return false;
  }
  if (node.acceptedHmrExports) {
    boundaries.push({
      boundary: node,
      acceptedVia: node,
      isWithinCircularImport: isNodeWithinCircularImports(node, currentChain),
    });
  } else if (!node.importers || !node.importers.size) {
    return true;
  }
  for (const importer of node.importers) {
    const subChain = currentChain.concat(importer);
    if (importer.acceptedHmrDeps && importer.acceptedHmrDeps.has(node)) {
      boundaries.push({
        boundary: importer,
        acceptedVia: node,
        isWithinCircularImport: isNodeWithinCircularImports(importer, subChain),
      });
      continue;
    }
    if (node.id && node.acceptedHmrExports && importer.importedBindings) {
      const importedBindingsFromNode = importer.importedBindings.get(node.id);
      if (importedBindingsFromNode && areAllImportsAccepted(importedBindingsFromNode, node.acceptedHmrExports))
        continue;
    }
    if (!currentChain.includes(importer) && propagateUpdate(importer, traversedModules, boundaries, subChain))
      return true;
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
      if (isNodeWithinCircularImports(importer, nodeChain, currentChain.concat(importer), traversedModules))
        return true;
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
  const srvCfg = (main.resolvedConfig && main.resolvedConfig.server) || (initial.config && initial.config.server) || {};
  const port = typeof srvCfg.port === "number" ? srvCfg.port : 0;
  const host = typeof srvCfg.host === "string" && srvCfg.host !== "localhost" ? srvCfg.host : "127.0.0.1";
  const s = new EventEmitter();
  let listening = false;
  s.on("listening", () => {
    listening = true;
  });
  // `upgrade` listeners are held (in registration order, prepend honored) and
  // replayed onto the real middleware server once it exists: Vite's shared
  // httpServer semantics for tunnel-style plugins.
  s._upgradeListeners = [];
  s._upgradeTarget = null;
  const wrap = (method) => {
    const orig = s[method].bind(s);
    s[method] = (event, cb) => {
      if (event === "listening" && listening) {
        cb();
        return s;
      }
      if (event === "upgrade") {
        if (s._upgradeTarget) return (s._upgradeTarget[method === "once" ? "once" : "on"](event, cb), s);
        s._upgradeListeners.push([method === "once" ? "once" : "on", cb]);
        return s;
      }
      return orig(event, cb);
    };
  };
  wrap("on");
  wrap("once");
  wrap("addListener");
  {
    const origPrepend = s.prependListener.bind(s);
    s.prependListener = (event, cb) => {
      if (event === "upgrade") {
        if (s._upgradeTarget) return (s._upgradeTarget.prependListener(event, cb), s);
        s._upgradeListeners.unshift(["on", cb]);
        return s;
      }
      return origPrepend(event, cb);
    };
    const origRemove = s.removeListener.bind(s);
    const remove = (event, cb) => {
      if (event === "upgrade") {
        if (s._upgradeTarget) return (s._upgradeTarget.removeListener(event, cb), s);
        const i = s._upgradeListeners.findIndex(([, fn]) => fn === cb);
        if (i !== -1) s._upgradeListeners.splice(i, 1);
        return s;
      }
      return origRemove(event, cb);
    };
    s.removeListener = remove;
    s.off = remove;
  }
  // Vite parity: null until the socket is bound; after that node reports the
  // REAL bound interface and port (auto-increment can move the port off the
  // configured one, and the interface is the bind's, not the config's). The
  // config fallbacks only serve the direct-drive node mode, which has no
  // announce.
  s.address = () => {
    if (!listening) return null;
    const bound = ojDevListenerPort ?? (port || null);
    if (!bound) return null;
    const address = ojDevListenerAddress ?? host;
    return { address, family: address.includes(":") ? "IPv6" : "IPv4", port: bound };
  };
  s.listen = () => s;
  s.close = (cb) => {
    if (typeof cb === "function") cb();
    return s;
  };
  Object.defineProperty(s, "listening", { get: () => listening });
  return s;
}

// An oj-backed stand-in for a Vite DevEnvironment (name, config with consumer,
// moduleGraph, hot, plugins, pluginContainer over the host's chains). The client
// stub shares server.moduleGraph; others get their own graph. Marked so oj's
// invalidation of runner-backed environments skips it.
// Vite's `server.environments.<name>` is the very environment the hooks see
// as `this.environment` (createServer inits them before configureServer): a
// container of that environment's, filled out to the DevEnvironment surface
// the stand-in below offers, its own graph, channel and plugins kept. Like
// the stand-in it is oj's (`__ojStub`), never a runner-backed Vite one.
function containerEnvironment(name, server) {
  const c =
    main.environment.name === name
      ? main
      : [...containers.values()].find((x) => x.durable && !x.buildStartOwner && x.environment.name === name);
  if (!c) return null;
  const environment = c.environment;
  const stub = stubEnvironment(name, server);
  for (const [key, value] of Object.entries(stub)) {
    if (environment[key] === undefined) environment[key] = value;
  }
  environment.pluginContainer = {
    ...stub.pluginContainer,
    resolveId: async (source, importer, options) => resolveIdFull(c, source, importer, options),
    load: async (id) => loadFull(c, id),
    transform: async (code, id) => ({ code: JSON.parse(await transform(c, code, id, null)).code }),
  };
  return environment;
}
function stubEnvironment(name, server) {
  const isClient = name === "client";
  const base = main.resolvedConfig ?? {};
  const config = isClient ? base : withResolvedDefaults(mergeConfigLite(base, (base.environments ?? {})[name] ?? {}));
  // Vite tags each environment's config with its consumer; the merge above
  // inherits the host environment's tag, so set it per environment.
  if (isClient) config.consumer = config.consumer ?? "client";
  else config.consumer = (base.environments ?? {})[name]?.consumer ?? "server";
  const mg = isClient ? moduleGraph : createModuleGraph({ seenIds: main.seenIds, environment: { name } });
  return {
    __ojStub: true,
    name,
    mode: "dev",
    config,
    logger: base.logger,
    moduleGraph: mg,
    hot: wsApi,
    watcher: server.watcher,
    plugins: main.plugins,
    depsOptimizer: undefined,
    pluginContainer: {
      buildStart: async () => {},
      close: async () => {},
      resolveId: async (source, importer, options) => resolveIdFull(main, source, importer, options),
      load: async (id) => loadFull(main, id),
      transform: async (code, id) => ({ code: JSON.parse(await transform(main, code, id, null)).code }),
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
// http-proxy-3's web-outgoing passes for the options the node pipe applies
// itself (passes/web-outgoing.js setRedirectHostRewrite + writeHeaders, and
// common.js toURL / rewriteCookieProperty), so `autoRewrite`, `hostRewrite`,
// `protocolRewrite`, `cookieDomainRewrite` and `cookiePathRewrite` behave as
// under Vite. Without any of them every header passes through untouched.
const PROXY_REDIRECT_RE = /^201|30(1|2|7|8)$/;
function proxyURL(url) {
  if (url instanceof URL) return url;
  if (url && typeof url === "object" && typeof url.href === "string") url = url.href;
  if (!url) url = "";
  if (typeof url != "string") url = `${url}`;
  if (url.startsWith("//")) url = `http://base.invalid${url}`;
  return new URL(url, "http://base.invalid");
}
function rewriteCookieProperty(header, config, property) {
  if (Array.isArray(header)) return header.map((h) => rewriteCookieProperty(h, config, property));
  return header.replace(new RegExp("(;\\s*" + property + "=)([^;]+)", "i"), (match, prefix, previous) => {
    let next;
    if (previous in config) next = config[previous];
    else if ("*" in config) next = config["*"];
    else return match;
    return next ? prefix + next : "";
  });
}
function proxyResponseHeaders(opts, req, statusCode, headers) {
  if (
    (opts.hostRewrite || opts.autoRewrite || opts.protocolRewrite) &&
    typeof headers.location === "string" &&
    PROXY_REDIRECT_RE.test(`${statusCode}`)
  ) {
    const target = proxyURL(opts.target);
    const u = proxyURL(headers.location);
    if (target.host == u.host) {
      if (opts.hostRewrite) u.host = opts.hostRewrite;
      else if (opts.autoRewrite) u.host = req.headers[":authority"] ?? req.headers["host"] ?? "";
      if (opts.protocolRewrite) u.protocol = opts.protocolRewrite;
      headers.location = u.toString();
    }
  }
  const cookieConfig = (v) => (typeof v === "string" ? { "*": v } : v && typeof v === "object" ? v : null);
  const domain = cookieConfig(opts.cookieDomainRewrite);
  const path = cookieConfig(opts.cookiePathRewrite);
  if (headers["set-cookie"] != null) {
    if (domain) headers["set-cookie"] = rewriteCookieProperty(headers["set-cookie"], domain, "domain");
    if (path) headers["set-cookie"] = rewriteCookieProperty(headers["set-cookie"], path, "path");
  }
  return headers;
}

// follow-redirects' rules, which http-proxy-3 uses for `followRedirects: true`:
// at most 21 hops; a 301/302 answering a POST, or a 303 answering anything
// but GET/HEAD, continues as a body-less GET; credentials and cookies are
// dropped when the next hop leaves the host (or its subdomains) or downgrades
// from https.
const FOLLOW_MAX_REDIRECTS = 21;
const FOLLOW_MAX_BODY = 10485760;
const FOLLOW_HEADER_FILTER = /^(?:(?:proxy-)?authorization|cookie)$/i;
function followRedirectStep(method, headers, statusCode, current, location) {
  const next = new URL(location, current);
  headers = { ...headers };
  let dropBody = false;
  if (
    ((statusCode === 301 || statusCode === 302) && method === "POST") ||
    (statusCode === 303 && !/^(?:GET|HEAD)$/.test(method))
  ) {
    method = "GET";
    dropBody = true;
    for (const k of Object.keys(headers)) if (/^content-/i.test(k)) delete headers[k];
  }
  const currentHost = headers.host || current.host;
  for (const k of Object.keys(headers)) if (/^host$/i.test(k)) delete headers[k];
  const sub = next.host.endsWith("." + currentHost);
  if ((next.protocol !== current.protocol && next.protocol !== "https:") || (next.host !== currentHost && !sub)) {
    for (const k of Object.keys(headers)) if (FOLLOW_HEADER_FILTER.test(k)) delete headers[k];
  }
  return { method, headers, url: next, dropBody };
}

function pipeTarget(target) {
  if (target instanceof URL) return target;
  if (typeof target === "string") {
    try {
      return new URL(target);
    } catch {
      return null;
    }
  }
  if (target && typeof target === "object") {
    if (typeof target.href === "string") {
      try {
        return new URL(target.href);
      } catch {}
    }
    let protocol = typeof target.protocol === "string" && target.protocol ? target.protocol : "http:";
    if (!protocol.endsWith(":")) protocol += ":";
    const hostOnly = typeof target.host === "string" ? target.host.split(":") : [];
    const hostname = target.hostname || hostOnly[0] || "";
    if (!hostname) return null;
    const port = target.port != null ? String(target.port) : hostOnly[1] || "";
    const pathname = target.pathname || target.path || "";
    try {
      return new URL(`${protocol}//${hostname}${port ? ":" + port : ""}${pathname}`);
    } catch {
      return null;
    }
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
      try {
        re = new RegExp(opts.rewrite.from);
      } catch (e) {
        process.stderr.write(
          `${OJ} plugin host: server.proxy["${context}"].rewrite.from is not a valid regex: ${(e && e.message) || e}\n`,
        );
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
  try {
    requirers.push(createRequire(createRequire(appRoot + "/package.json").resolve("vite")));
  } catch {}
  try {
    requirers.push(createRequire(appRoot + "/package.json"));
  } catch {}
  try {
    requirers.push(createRequire(import.meta.url));
  } catch {}
  for (const req of requirers) {
    for (const name of ["http-proxy-3", "http-proxy"]) {
      try {
        const mod = await import(pathToFileURL(req.resolve(name)).href);
        const lib = mod.default ?? mod;
        if (lib && typeof lib.createProxyServer === "function") {
          httpProxyLib = lib;
          break;
        }
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
        try {
          opts.configure(proxy, opts);
        } catch (e) {
          process.stderr.write(
            `${OJ} plugin host: server.proxy["${context}"].configure threw: ${(e && e.message) || e}\n`,
          );
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
            try {
              res.writeHead(502, { "Content-Type": "text/plain" }).end();
            } catch {}
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
    const PIPE_UNSUPPORTED = ["configure"];
    for (const [context, opts] of contexts) {
      const present = PIPE_UNSUPPORTED.filter((k) => opts[k] != null && opts[k] !== false);
      if (present.length) {
        process.stderr.write(
          `${OJ} plugin host: server.proxy["${context}"] ${present.join(", ")} need http-proxy (bundled into Vite, not resolvable here); the built-in proxy does not apply them\n`,
        );
      }
    }
    process.stderr.write(`${OJ} plugin host: server.proxy active (${contexts.length} route(s))\n`);
  }

  // Faithful node http/https pipe (used when http-proxy is not resolvable).
  const pipeForward = async (opts, req, res) => {
    const target = pipeTarget(opts.target);
    if (!target) {
      res.statusCode = 502;
      res.end(
        `oj proxy: invalid target ${typeof opts.target === "object" ? JSON.stringify(opts.target) : opts.target}`,
      );
      return;
    }
    // Mirror http-proxy's error handling: once the response has started the
    // stream is committed, so DESTROY the socket rather than appending an
    // error string onto the partial body (which would corrupt the response).
    // Only synthesize a 502 when nothing has been sent yet.
    const fail = (e) => {
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
    };
    const headers = { ...req.headers };
    // http-proxy's XHeaders (web-incoming.js), before changeOrigin swaps the
    // Host: the browser's address, port and protocol appended to any
    // x-forwarded-* already there, and x-forwarded-host when none came in.
    if (opts.xfwd) {
      const encrypted = !!req.socket?.encrypted;
      const hostHeader = req.headers[":authority"] || req.headers.host;
      const port = hostHeader ? hostHeader.match(/:(\d+)/) : "";
      const values = {
        for: req.connection?.remoteAddress || req.socket?.remoteAddress,
        port: port ? port[1] : encrypted ? "443" : "80",
        proto: encrypted ? "https" : "http",
      };
      for (const h of ["for", "port", "proto"]) {
        const key = "x-forwarded-" + h;
        headers[key] = (headers[key] || "") + (headers[key] ? "," : "") + values[h];
      }
      headers["x-forwarded-host"] = headers["x-forwarded-host"] || headers[":authority"] || headers.host || "";
    }
    // http-proxy's changeOrigin: send the target's host, not the dev server's
    // (otherwise the browser's Host is forwarded unchanged, as http-proxy does).
    if (opts.changeOrigin) headers.host = target.host;
    // Prepend a target base path (http-proxy prependPath) to the (already
    // rewritten) request url.
    const base = target.pathname && target.pathname !== "/" ? target.pathname.replace(/\/$/, "") : "";
    // `followRedirects` replays the request on each hop, so its body is held
    // (as follow-redirects does, up to its 10 MiB maxBodyLength); otherwise
    // it streams through and is never buffered whole.
    const follow = !!opts.followRedirects;
    let body = null;
    if (follow) {
      const chunks = [];
      let size = 0;
      try {
        for await (const chunk of req) {
          size += chunk.length;
          if (size > FOLLOW_MAX_BODY) throw new Error("Request body larger than maxBodyLength limit");
          chunks.push(chunk);
        }
      } catch (e) {
        return fail(e);
      }
      body = Buffer.concat(chunks);
    }
    let hop = {
      method: req.method,
      headers,
      protocol: target.protocol,
      hostname: target.hostname,
      port: target.port,
      path: base + req.url,
      url: (() => {
        try {
          return new URL(`${target.protocol}//${target.host}${base}${req.url}`);
        } catch {
          return target;
        }
      })(),
    };
    let redirects = 0;
    let upstream = null;
    // Client disconnect / response error: abort the upstream so its socket is
    // not leaked, and swallow res errors (EPIPE/ECONNRESET) so they do not
    // surface as an unhandled error on the host.
    const abortUpstream = () => {
      if (upstream && !upstream.destroyed) upstream.destroy();
    };
    res.on("close", () => {
      if (!res.writableFinished) abortUpstream();
    });
    res.on("error", abortUpstream);
    req.on("error", abortUpstream);
    const send = () => {
      const isHttps = hop.protocol === "https:";
      const options = {
        protocol: hop.protocol,
        hostname: hop.hostname,
        port: hop.port || (isHttps ? 443 : 80),
        method: hop.method,
        path: hop.path,
        headers: hop.headers,
      };
      // http-proxy's secure:false accepts self-signed dev certificates.
      if (isHttps && opts.secure === false) options.rejectUnauthorized = false;
      upstream = (isHttps ? https : http).request(options, (r) => {
        const status = r.statusCode || 502;
        if (follow && r.headers.location && status >= 300 && status < 400) {
          r.resume();
          if (++redirects > FOLLOW_MAX_REDIRECTS) return fail(new Error("Maximum number of redirects exceeded"));
          const step = followRedirectStep(hop.method, hop.headers, status, hop.url, r.headers.location);
          if (step.dropBody) body = null;
          hop = {
            method: step.method,
            headers: step.headers,
            protocol: step.url.protocol,
            hostname: step.url.hostname,
            port: step.url.port,
            path: step.url.pathname + step.url.search,
            url: step.url,
          };
          return send();
        }
        // Pass the upstream status and every header through (Set-Cookie,
        // Location, content-type ...) as http-proxy does, with only the
        // rewrites the entry asks for, but STRIP oj's internal signal headers
        // so a real upstream that happens to emit them cannot be misread as
        // the host stack's fallthrough (only the host's runStack may set these).
        const out = proxyResponseHeaders(opts, req, status, { ...r.headers });
        delete out["x-oj-fallthrough"];
        delete out["x-oj-rewritten-url"];
        res.writeHead(status, out);
        r.pipe(res);
      });
      upstream.on("error", fail);
      if (follow) upstream.end(body ?? undefined);
      else req.pipe(upstream);
    };
    try {
      send();
    } catch (e) {
      fail(e);
    }
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

// Vite runs the server-level setup once per dev server (server/index.ts:
// configureServer hooks, the proxy, the environments), on plugin instances
// every environment shares; a per-environment plugin's configureServer is
// ignored (plugin.ts resolveEnvironmentPlugins). The dev server's host is
// the client's, and its other environments are containers on it, so the
// setup runs once, there; a host of the ssr environment (a build's) runs none.
const ownsServer = hostEnvName === "client";

async function setupConfigureServer() {
  const stack = [];
  function runStack(req, res, done) {
    let i = 0;
    const next = (err) => {
      while (i < stack.length) {
        const entry = stack[i++];
        const path = entry.route === "/" ? null : entry.route;
        const fn = entry.handle;
        if (path && !req.url.startsWith(path)) continue;
        if (fn.length >= 4 !== (err != null)) continue;
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
      try {
        res.setHeader("x-oj-rewritten-url", req.url);
      } catch {}
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
  // connect stack entries are { route, handle } (plugins walk .stack reading
  // handle.name); mirror that shape.
  middlewares.use = (a, b) => {
    if (typeof a === "function") stack.push({ route: "/", handle: a });
    else stack.push({ route: a, handle: b });
  };
  middlewares.stack = stack;
  const noop = () => {};
  const fileWatcher = Object.assign(new EventEmitter(), { add: noop, unwatch: noop, close: noop });
  const server = {
    config: main.resolvedConfig,
    middlewares,
    httpServer: stubHttpServer(),
    // Vite: "will be set on listen" (server/index.ts), null until then.
    resolvedUrls: null,
    ws: wsApi,
    hot: wsApi,
    watcher: fileWatcher,
    moduleGraph,
    // Vite's printUrls (logger.ts printServerUrls), uncolored: the host's
    // logger is a stderr shim, not a TTY. Same pre-listen error as Vite's.
    printUrls() {
      if (!server.resolvedUrls) {
        throw new Error("Cannot print server URLs before server.listen is called.");
      }
      const logger = server.config && server.config.logger;
      const info =
        logger && typeof logger.info === "function"
          ? logger.info.bind(logger)
          : (msg) => process.stderr.write(`${msg}\n`);
      for (const url of server.resolvedUrls.local) info(`  ➜  Local:   ${url}`);
      server.resolvedUrls.network.forEach((url, index) => {
        const interfaceName = (server.resolvedUrls.networkInterfaceNames ?? [])[index];
        info(`  ➜  Network: ${url}${interfaceName ? `  ${interfaceName}` : ""}`);
      });
      const optionsHost = (server.config && server.config.server && server.config.server.host) ?? undefined;
      if (server.resolvedUrls.network.length === 0 && optionsHost === undefined) {
        info("  ➜  Network: use --host to expose");
      }
    },
    // Vite's bindCLIShortcuts returns early unless stdin is a TTY
    // (shortcuts.ts); the host's stdin is oj's RPC channel, never a TTY, and
    // oj owns the terminal. Plugins wrap it (`server.bindCLIShortcuts.bind`)
    // to add their own shortcuts, so it must exist.
    bindCLIShortcuts() {},
    // Vite restarts the dev server (re-reading the config); oj re-execs itself,
    // which is how it already handles a config-file change.
    restart: async () => ojServerEvent("restart"),
    close: async () => {},
    transformIndexHtml: async (url, html, originalUrl) => devTransformIndexHtml(url, html, originalUrl),
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
  // builds from what the app's Vite resolved. Extraction
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
    Object.values(cfg.environments ?? {}).some((e) => e && e.dev && typeof e.dev.createEnvironment === "function");
  const extractionSaysBacked = initial.runnerBacked === true;
  const hostSeesDeclaration =
    !extractionSaysBacked && [userViteConfig, initial.config, pluginConfigDelta].some(declaresRunnerEnvironment);
  const runnerDeclared = extractionSaysBacked || hostSeesDeclaration;
  const runnerBackedSource = extractionSaysBacked
    ? "config extraction"
    : initial.runnerBacked === false
      ? "host declaration check (extraction said no)"
      : "host declaration check";
  if (runnerDeclared && ownsServer) {
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
  if (runnerDeclared && ownsServer && !runnerEnvironmentsBuilt) {
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
    const envNames = new Set(["client", "ssr", ...Object.keys(main.resolvedConfig?.environments ?? {})]);
    for (const name of envNames)
      server.environments[name] = containerEnvironment(name, server) ?? stubEnvironment(name, server);
  }
  devServer = server;
  const post = [];
  for (const { p, fn } of ownsServer ? pluginsWithHook(main, "configureServer") : []) {
    let r;
    initStage(`configureServer:${p.name ?? "?"}`);
    try {
      r = await fn.call(ctxFor(main, p), server);
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
    userViteConfig?.server?.proxy ?? initial.config?.server?.proxy ?? main.resolvedConfig?.server?.proxy;
  if (ownsServer && proxyConfig && typeof proxyConfig === "object") {
    try {
      const proxyMw = await createProxyMiddleware(proxyConfig, initial.config?.root ?? process.cwd());
      if (proxyMw) middlewares.use(proxyMw);
    } catch (e) {
      process.stderr.write(`${OJ} plugin host: failed to set up server.proxy: ${(e && e.stack) || e}\n`);
    }
  }
  // Vite's baseMiddleware sits here, after the proxy and before the POST
  // hooks: PRE middleware sees the browser's URL, POST middleware the URL
  // with the base stripped. oj answers the root redirect and unbased
  // requests itself, so only the strip runs (Vite's middleware mode).
  const devBase = ownsServer ? ojDevBase(main.resolvedConfig ?? initial.config ?? {}) : "/";
  if (devBase !== "/") {
    middlewares.use(function viteBaseMiddleware(req, _res, next) {
      const pathname = req.url.split(/[?#]/)[0];
      if (pathname.startsWith(devBase)) req.url = ojStripBase(req.url, devBase);
      next();
    });
  }
  for (const fn of post) {
    try {
      await fn();
    } catch (e) {
      if (!ojStartMode) throw e;
      process.stderr.write(`${OJ} plugin host: post configureServer skipped: ${(e && e.message) || e}\n`);
    }
  }
  // Setup is over: later registrations are the session's own discoveries.
  bootRegistrationsOpen = false;
  ojConfigureServerDone = true;
  // The middleware server exists as soon as configureServer registered
  // anything, like Vite's connect stack exists at createServer — never
  // behind the emit below, whose buildStart gate may be slow.
  await ensureConfigureServerMiddleware();
  // Vite's listen() comes after createServer (configureServer included), so
  // `once("listening")` handlers registered above fire when the socket is
  // really bound. In-process that bind is oj's, announced through the
  // `serverListening` hook — possibly already arrived (emit now then). Under
  // plain node (the direct-drive unit tests) no announce exists, so the emit
  // stays here like the old host's.
  if (!ojEnginePost || ojDevListenerBound) {
    await ojEmitListening(server);
    // Listening callbacks may have registered the first middleware or
    // upgrade listeners; on this path no later announce re-checks.
    await ensureConfigureServerMiddleware();
  }
}

// Vite's resolveBaseUrl in serve: "", "./" and other relative bases are the
// root, and a URL base keeps only its path.
function ojDevBase(cfg) {
  const raw = cfg.rawBase ?? cfg.base;
  if (typeof raw !== "string" || raw === "" || raw[0] === ".") return "/";
  const base = new URL(raw, "http://vite.dev").pathname;
  return base[0] === "/" ? base : `/${base}`;
}

// Vite's stripBase.
function ojStripBase(url, base) {
  if (url === base) return "/";
  const devBase = base.endsWith("/") ? base : `${base}/`;
  return url.startsWith(devBase) ? url.slice(devBase.length - 1) : url;
}

function ojPushServeInfo() {
  ctl({ ojServeInfo: { middlewarePort, runnerEnvironments: runnerEnvironmentsBuilt } });
}

// The forwarding middleware server, created once plugin middleware or upgrade
// listeners exist. Usually both are known by configureServer end; a plugin
// that only registers them inside a "listening" callback (in-process: fired
// at oj's bind announce) gets the server created then, with the serve info
// re-pushed so Rust starts forwarding.
// The browser's connection, which the loopback hop carries as
// x-oj-remote-address / x-oj-encrypted: back on req.socket, where Vite's
// plugins and http-proxy's `xfwd` read it (`remoteAddress`, `encrypted`,
// `pair`).
// A kept-alive loopback socket carries many requests, so each one sets (or
// clears) its own values.
function restoreClientSocket(req) {
  const remote = req.headers["x-oj-remote-address"];
  const encrypted = req.headers["x-oj-encrypted"] === "1";
  delete req.headers["x-oj-remote-address"];
  delete req.headers["x-oj-encrypted"];
  const socket = req.socket;
  if (!socket) return;
  try {
    if (typeof remote === "string" && remote) {
      Object.defineProperty(socket, "remoteAddress", { value: remote, configurable: true });
    } else {
      delete socket.remoteAddress;
    }
    // A TLS socket under Vite. http-proxy reads `encrypted || pair`;
    // http-proxy-3 (what Vite bundles) `instanceof TLSSocket && encrypted`,
    // else `pair`, the legacy SecurePair mark no current Node sets. So both
    // see the browser's TLS for `xfwd` (x-forwarded-proto, the default port).
    for (const key of ["encrypted", "pair"]) {
      if (encrypted) Object.defineProperty(socket, key, { value: true, configurable: true });
      else delete socket[key];
    }
  } catch {}
}

let ojMiddlewareServerStarted = false;
async function ensureConfigureServerMiddleware() {
  if (ojMiddlewareServerStarted || !devServer) return;
  const server = devServer;
  const middlewares = server.middlewares;
  const stack = middlewares.stack;
  const fileWatcher = server.watcher;
  const upgradeListeners = server.httpServer._upgradeListeners || [];
  if (stack.length === 0 && upgradeListeners.length === 0) return;
  ojMiddlewareServerStarted = true;

  const srv = http.createServer((req, res) => {
    // The browser's Host travels as x-oj-host (hyper owns the loopback Host):
    // middlewares read req.headers.host as they do under Vite.
    if (typeof req.headers["x-oj-host"] === "string") {
      req.headers.host = req.headers["x-oj-host"].split(",")[0].trim() || req.headers.host;
      delete req.headers["x-oj-host"];
    }
    restoreClientSocket(req);
    if (req.method === "POST" && req.url === "/__oj_invalidate") {
      let body = "";
      req.on("data", (c) => (body += c));
      req.on("end", async () => {
        let changes = [];
        let resync = false;
        try {
          const parsed = JSON.parse(body || "{}");
          resync = parsed.resync === true;
          changes = parsed.changes || (parsed.paths || []).map((p) => ({ path: p, type: "update" }));
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
            enqueueResync(() => {
              resyncPending = false;
              resyncEnvironments(server.environments);
              ctl({ ojResyncDone: true });
            }).catch(() => {
              resyncPending = false;
            });
          }
          res.statusCode = 204;
          res.end();
          return;
        }
        // Answer only once the invalidation is done, so the Rust side's POST
        // completing means a next request cannot be served from stale modules.
        // The reply says how many changes matched a runner-backed graph
        // (null: the walk threw, the caller must assume a hit): the Rust side
        // reloads the browser only when something actually served changed.
        const matchedCount = await invalidateEnvironments(server.environments, fileWatcher, changes);
        res.statusCode = 200;
        res.setHeader("content-type", "application/json");
        res.end(JSON.stringify({ matched: matchedCount === null ? null : matchedCount || 0 }));
      });
      return;
    }
    middlewares(req, res);
  });
  // The Rust listener relays unclaimed browser upgrades here as real upgrade
  // requests; listeners registered on the stub attach in registration order.
  for (const [method, fn] of upgradeListeners) srv[method]("upgrade", fn);
  server.httpServer._upgradeTarget = srv;
  await new Promise((resolve) => srv.listen(0, "127.0.0.1", resolve));
  middlewarePort = srv.address().port;
  process.stderr.write(`${OJ} plugin host: configureServer middleware on :${middlewarePort}\n`);
  // A late creation (post-boot listening callback) must reach Rust: the boot
  // push already went out without this port.
  if (env.command !== "build") ojPushServeInfo();
}
// Under plain node the push is re-sent until the driver ACKs ({ ojServeInfoAck }
// on stdin): the serve info is a one-shot, state-bearing push, and a copy
// spliced by a plugin's partial stdout write must not be lost forever. Bounded
// so a driver that never ACKs is not spammed indefinitely. In-process the push
// channel is a value delivery that cannot be spliced: one push, no ACK.
let serveInfoRepush = null;
function stopServeInfoRepush() {
  if (serveInfoRepush) clearInterval(serveInfoRepush);
  serveInfoRepush = null;
}
if (env.command !== "build") {
  await setupConfigureServer();
  // Push the serve info the moment it exists (like the {ojWs} pushes): the
  // hook entry point only becomes callable after every top-level await, so on
  // a slow boot (many plugins, Miniflare) Rust's boot-time RPCs cannot be
  // answered and the worker path would silently never activate. The push
  // reaches Rust whenever the host comes up, however late, and Rust flips to
  // the middleware then (it also flips Rust's "initialized" gate for RPC
  // timeouts).
  ojPushServeInfo();
  if (!ojEnginePost) {
    let repushes = 0;
    serveInfoRepush = setInterval(() => {
      if (++repushes > 120) return stopServeInfoRepush();
      ojPushServeInfo();
    }, 1000);
  }
}

// Vite's plugin order puts `vite:oxc` (the TS/JSX strip) after the user's
// enforce:"pre" plugins and before the normal and post ones, so only a pre
// plugin (or a hook with `order: "pre"`) ever sees TypeScript or JSX; every
// later transform receives JS it can `this.parse`. oj compiles natively after
// the whole chain, so the strip runs here, at the same slot, and only when a
// later plugin actually claims the module. The options are the resolved
// config's `oxc` as vite:oxc passes them, with refresh off: oj's compile adds
// Fast Refresh itself.
const OXC_STRIP_RE = /\.(m?ts|[jt]sx)$/;
function runsBeforeOxcStrip(p, rank) {
  return rank < 0 || (rank === 0 && enforceRank(p) < 0);
}
// vite:oxc's own filter: createFilter(include || ts-jsx-default, exclude ||
// /\.js$/), so an app's oxc.include/exclude decide which modules the strip
// touches (an excluded module reaches normal/post plugins raw, as in Vite).
function oxcStripMatches(c, id) {
  const clean = id.split("?")[0];
  const resolvedConfig = c.resolvedConfig;
  if (c.oxcStripFilter === undefined && resolvedConfig) {
    const oxc = resolvedConfig.oxc;
    if (oxc === false) c.oxcStripFilter = () => false;
    else if (viteCreateFilter) {
      const { include, exclude } = oxc && typeof oxc === "object" ? oxc : {};
      c.oxcStripFilter = viteCreateFilter(include || OXC_STRIP_RE, exclude || /\.js$/);
    } else c.oxcStripFilter = (i) => OXC_STRIP_RE.test(i);
  }
  return c.oxcStripFilter ? c.oxcStripFilter(clean) : OXC_STRIP_RE.test(clean);
}
function oxcStripOptions(c) {
  const resolvedConfig = c.resolvedConfig;
  const oxc = resolvedConfig && resolvedConfig.oxc;
  if (oxc === false) return null;
  const {
    jsxInject: _i,
    include: _in,
    exclude: _ex,
    jsxRefreshInclude: _ri,
    jsxRefreshExclude: _re,
    ...opts
  } = oxc && typeof oxc === "object" ? oxc : {};
  const isProduction = resolvedConfig?.isProduction ?? process.env.NODE_ENV === "production";
  const jsx = typeof opts.jsx === "string" ? opts.jsx : { development: !isProduction, ...opts.jsx, refresh: false };
  return { ...opts, jsx, sourcemap: true };
}

// A stylesheet's slot in Vite's transform order, around its core CSS plugins:
// "pre" hooks (enforce or order "pre") see the source before vite:css compiles
// it, "normal" ones the compiled CSS before vite:css-post turns it into JS,
// "post" ones (enforce or order "post") that JS module.
function cssPhaseOf(p, rank) {
  if (rank < 0 || (rank === 0 && enforceRank(p) < 0)) return "pre";
  if (rank > 0 || enforceRank(p) > 0) return "post";
  return "normal";
}

// `phase` (a cssPhaseOf slot) runs only that slot's hooks; without one, the
// whole chain. Module info is final after the whole chain or the "post" slot.
async function transform(c, code, id, resolvedJson, phase) {
  const bucket = new Set();
  const chunkBucket = [];
  let resolveMap = null;
  if (resolvedJson) {
    try {
      resolveMap = new Map(Object.entries(JSON.parse(resolvedJson)));
    } catch {}
  }
  const runLoop = async () => {
    if (id) c.seenIds.add(id);
    let current = code;
    const maps = [];
    // Who returned each map, so a bundle can name a map it cannot read.
    const mapPlugins = [];
    // Rollup's missing map: a plugin that returns code without `map` breaks
    // the chain (`map: null` does not). Only a bundle reads it.
    let mapBroken = null;
    const transformOptions = { ssr: c.environment.name === "ssr" };
    let stripped = !viteTransformWithOxc || !id || id.startsWith("\0") || !oxcStripMatches(c, id);
    // Vite sorts a hook's plugins by the hook's own `order` (pre, normal, post)
    // on top of the plugin's enforce; getSortedPluginsByHook applies to transform
    // like to every other hook.
    for (const { p, fn: handler, rank } of pluginsWithHook(c, "transform")) {
      if (phase && cssPhaseOf(p, rank) !== phase) continue;
      // The strip runs BEFORE this plugin's filter is evaluated: Vite
      // evaluates a hook's filter against the code the hook would receive
      // (post-oxc for normal/post plugins), so a `filter.code` that matches
      // only compiled output must see the stripped source. The id-only
      // pre-check keeps the strip lazy for plugins whose id filter can never
      // match this module.
      if (!stripped && !runsBeforeOxcStrip(p, rank) && hookTransformMatches(p.transform, id, null, true)) {
        stripped = true;
        const opts = oxcStripOptions(c);
        if (opts) {
          let r;
          try {
            r = await viteTransformWithOxc(current, id, opts);
          } catch (e) {
            throw decoratePluginError(e, { name: "vite:oxc" }, id);
          }
          current = r.code;
          if (r.map != null) {
            maps.push(typeof r.map === "string" ? r.map : JSON.stringify(r.map));
            mapPlugins.push("vite:oxc");
          }
        }
      }
      if (!hookTransformMatches(p.transform, id, current)) continue;
      let r;
      try {
        r = await handler.call(ctxFor(c, p), current, id, transformOptions);
      } catch (e) {
        throw decoratePluginError(e, p, id);
      }
      if (r == null) continue;
      if (typeof r === "string") {
        current = r;
        mapBroken ??= p.name ?? "?";
        continue;
      }
      if (r.code != null) current = r.code;
      if (r.map != null) {
        maps.push(typeof r.map === "string" ? r.map : JSON.stringify(r.map));
        mapPlugins.push(p.name ?? "?");
      } else if (r.code != null && r.map === undefined) mapBroken ??= p.name ?? "?";
      updateModuleInfo(c, id, r);
    }
    const result = () =>
      JSON.stringify({
        code: current,
        watchFiles: [...bucket],
        maps,
        mapPlugins,
        mapBroken,
        emittedChunks: chunkBucket,
      });
    if (phase && phase !== "post") return result();
    const prior = c.moduleInfoCache.get(id);
    const info = makeModuleInfo(
      id,
      current,
      prior
        ? {
            meta: prior.meta,
            moduleSideEffects: prior.moduleSideEffects,
            syntheticNamedExports: prior.syntheticNamedExports,
          }
        : null,
    );
    c.moduleInfoCache.set(id, info);
    c.seenIds.add(id);
    // A Start build's bundler reports every parsed module itself (bundleModuleParsed).
    if (!bundleBuildOf(c))
      for (const { p, fn } of pluginsWithHook(c, "moduleParsed")) await fn.call(ctxFor(c, p), info);
    return result();
  };
  return transformWatchStore.run(bucket, () =>
    transformEmitStore.run(chunkBucket, () => transformResolveStore.run(resolveMap, runLoop)),
  );
}

async function replayModuleParsed(c, id) {
  if (id) c.seenIds.add(id);
  const info = c.moduleInfoCache.get(id) ?? makeModuleInfo(id, "");
  c.moduleInfoCache.set(id, info);
  for (const { fn } of pluginsWithHook(c, "moduleParsed")) await fn.call(contextOf(c), info);
  return null;
}

const anyModuleParsed = (c) => pluginsWithHook(c, "moduleParsed").length > 0;
// A Start build: rolldown drives the hooks, and its module_parsed reports
// every module it parses (node_modules included) with the resolved imports,
// Rollup's ModuleInfo, and the code as JS (Vite's vite:oxc lowers TS and JSX
// in a transform, so plugins never read either).
const bundleBuildOf = (c) => c.bundle && command === "build";
async function bundleModuleParsed(c, infoJson) {
  const reported = JSON.parse(infoJson);
  const prior = c.moduleInfoCache.get(reported.id);
  const info = makeModuleInfo(reported.id, reported.code ?? "", {
    ...(prior
      ? {
          meta: prior.meta,
          moduleSideEffects: prior.moduleSideEffects,
          syntheticNamedExports: prior.syntheticNamedExports,
        }
      : {}),
    importedIds: reported.importedIds ?? [],
    dynamicallyImportedIds: reported.dynamicallyImportedIds ?? [],
    importers: reported.importers ?? [],
    dynamicImporters: reported.dynamicImporters ?? [],
    exports: reported.exports ?? null,
    isEntry: !!reported.isEntry,
  });
  c.moduleInfoCache.set(info.id, info);
  c.seenIds.add(info.id);
  for (const { p, fn } of pluginsWithHook(c, "moduleParsed")) await fn.call(ctxFor(c, p), info);
  return null;
}

// Vite's builder with `sharedPlugins`: every environment builds on these
// plugin instances. The next environment's build starts afresh: its own
// plugin set (applyToEnvironment), buildStart, module graph, emitted files
// and closeBundle.
async function switchEnvironment(c, name) {
  await resolveEnvironment(c, name);
  c.buildStartRun = null;
  c.closeBundleDone = false;
  c.emitted.length = 0;
  c.moduleInfoCache.clear();
  c.seenIds.clear();
  return null;
}

// A worker entry's bundle (Vite's bundleWorkerEntry) gets its own container,
// made as Vite's createWorkerPlugins makes the worker's resolved config:
// `worker.plugins` called afresh on the app's own config, in this realm, so its
// plugins share module state with the app's as they do under Vite; their
// config hooks over the user config, configResolved with the resolved config
// marked `isWorker` with the bundle chain, then applyToEnvironment over a
// `client` environment of that config. The reply is the container's id.
let workerPluginsArrayWarned = false;
async function openWorkerContainer(parent, bundleChain) {
  // Vite reads `worker.plugins` off the config its config hooks returned: a
  // plugin's own value, else the user's. Not hookedUserConfig's, which sits on
  // Vite's resolved config, whose `worker.plugins` is Vite's own resolver.
  let create = (parent.configDelta ?? pluginConfigDelta).worker?.plugins ?? userViteConfig?.worker?.plugins;
  if (Array.isArray(create)) {
    const list = create;
    create = () => list;
    if (!workerPluginsArrayWarned) {
      workerPluginsArrayWarned = true;
      process.stderr.write(
        `${OJ} worker.plugins is now a function that returns an array of plugins. Please update your Vite config accordingly.\n`,
      );
    }
  }
  const raw = await flattenPlugins(typeof create === "function" ? create() : []);
  const workerPlugins = hostedPlugins(raw, userViteConfig, parent);
  const c = createContainer(nextContainerId++, { name: "client", mode: environment.mode }, null);
  c.leaveOut = parent.leaveOut;
  c.bundle = parent.bundle;
  c.topLevelPlugins = workerPlugins;
  c.rawPlugins = raw;
  const workerConfig = await runConfigHook(
    mergeConfigLite({}, parent.hookedConfig ?? hookedUserConfig),
    workerPlugins,
    {
      container: c,
    },
  );
  const resolved = {
    ...workerConfig,
    ...parent.resolvedConfig,
    isWorker: true,
    mainConfig: parent.resolvedConfig,
    bundleChain,
  };
  // As the app's: the natively run plugins stay listed (hooks unrun).
  const native = raw.filter((p) => OJ_NATIVE_PLUGIN_NAMES.has(p && p.name));
  resolved.plugins = withCssPostShim(workerPlugins.concat(native), cssPostShimInto(c.pluginCss));
  await runConfigResolved(workerPlugins, resolved, c);
  c.resolvedConfig = resolved;
  if (parent.ojConfig) c.ojConfig = { ...workerConfig, ...parent.ojConfig, isWorker: true, bundleChain };
  await resolveEnvironment(c, "client");
  containers.set(c.id, c);
  logActivePlugins(c.plugins);
  return c.id;
}

// Vite's builder.buildApp: every buildApp hook over the top-level plugins, in
// hook order, with the config builder's own (TanStack Start's, which builds
// every environment: oj's builds, the `buildEnvironments` reverse RPC) run
// just before the first post-order hook. `builder.environments` lists every
// configured environment; Rust names the ones it builds (`builtJson`: name ->
// { outDir, input }) and builds any of them a hook asks for, once. A throwing
// hook fails the build, as in Vite, unless a config-phase hook was skipped.
async function buildApp(builtJson) {
  const built = JSON.parse(builtJson || "{}");
  const names = new Set(["client", "ssr", ...Object.keys(main.resolvedConfig?.environments ?? {})]);
  const environments = {};
  for (const name of names) {
    const own = main.resolvedConfig?.environments?.[name] ?? {};
    const config = withResolvedDefaults(mergeConfigLite(main.resolvedConfig ?? {}, own));
    const ojBuild = built[name];
    if (ojBuild) {
      config.build = {
        ...config.build,
        outDir: ojBuild.outDir,
        rollupOptions: { ...config.build?.rollupOptions, input: ojBuild.input },
        rolldownOptions: { ...config.build?.rolldownOptions, input: ojBuild.input },
      };
    }
    environments[name] = { name, config, isBuilt: false };
  }
  let buildingAll = null;
  const buildEnvironments = () =>
    (buildingAll ??= asyncRpc("buildEnvironments", []).then(() => {
      for (const name of Object.keys(built)) if (environments[name]) environments[name].isBuilt = true;
    }));
  const builder = {
    config: main.resolvedConfig,
    environments,
    async build(environment) {
      const name = typeof environment === "string" ? environment : environment?.name;
      if (!built[name]) {
        const own = Object.keys(built).join(" and ");
        throw new Error(
          `oj build builds the ${own} environments; the "${name}" environment a plugin builds is not supported yet`,
        );
      }
      await buildEnvironments();
    },
    async runDevTools() {},
  };
  const context = minimalContext({ watchMode: false });
  const hooks = main.topLevelPlugins
    .filter((p) => !isReimplemented(p && p.name))
    .map((p) => ({ p, fn: hookFn(p, "buildApp"), rank: hookOrderRank(p.buildApp) }))
    .filter((h) => h.fn)
    .sort((a, b) => a.rank - b.rank);
  let ran = false;
  for (const { p, fn } of hooks) {
    if (!ran && p.buildApp && typeof p.buildApp === "object" && p.buildApp.order === "post") {
      ran = true;
      await buildEnvironments();
    }
    try {
      await fn.call(context, builder);
    } catch (e) {
      if (!main.configHookSkipped) throw decoratePluginError(e, p);
      process.stderr.write(
        `${OJ} plugin host: buildApp(${p.name ?? "?"}) failed (skipped): ${(e && e.message) || e}\n`,
      );
    }
  }
  if (!ran) await buildEnvironments();
  return null;
}

// Vite's resolved `build.emitAssets`: `build.ssrEmitAssets` for ssr, then the
// environment's own setting, then the top-level `build` every environment
// inherits, else true on the client only.
function environmentEmitAssets(c) {
  const cfg = c.resolvedConfig ?? {};
  const name = c.environment.name;
  if (name === "ssr" && cfg.build?.ssrEmitAssets !== undefined) return !!cfg.build.ssrEmitAssets;
  const own = cfg.environments?.[name]?.build?.emitAssets ?? cfg.build?.emitAssets;
  return own === undefined ? c.environment.config.consumer === "client" : !!own;
}
// What vite:asset keeps of an environment's assets without emitAssets.
const keptWithoutEmitAssets = (fileName) => /ssr-manifest\.json$/.test(fileName) || /\.[cm]?js\.map$/.test(fileName);
// transformIndexHtml accepts a legacy `transform` key that generic hookHandler
// does not; this is the one unwrap both the runner and hasTransformIndexHtml use.
function htmlHookFn(p) {
  const hook = p && p.transformIndexHtml;
  const fn = typeof hook === "function" ? hook : (hook?.handler ?? hook?.transform);
  return typeof fn === "function" ? fn : null;
}

// A container of an environment the app's Vite runs for real (a plugin's
// dev.createEnvironment, the Cloudflare shape) is not a second one: that
// environment's own container runs its hooks, as in Vite's single pass.
function runnerOwns(c) {
  if (!c.startAfter || !runnerEnvironmentsBuilt) return false;
  const real = devServer?.environments?.[c.environment.name];
  return !!real && !real.__ojStub && real !== c.environment;
}
async function watchChange(c, id, event) {
  if (runnerOwns(c)) return null;
  for (const { p, fn } of pluginsWithHook(c, "watchChange")) {
    if (!runsInDevEnvironment(c, p, "perEnvironmentWatchChangeDuringDev")) continue;
    await fn.call(ctxFor(c, p), id, { event });
  }
  return null;
}
// Vite's dev rule for buildStart, buildEnd and watchChange: every plugin runs
// them on the client environment, another environment's container only the
// plugins that ask for it per environment (`flag`, or the server option).
function runsInDevEnvironment(c, p, flag) {
  return !c.startAfter || !!c.resolvedConfig?.server?.[flag] || !!p[flag];
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
// A hook `filter`'s include patterns as oj's Rust-side gates take them
// (HookFilter::from_json, which may only over-approximate the host's own
// matching): a RegExp as its source and flags, a string id pattern as Vite's
// cwd-joined picomatch glob, a string code pattern as a literal (Vite's
// `code.includes`). null when a pattern is neither, so the gate fails open.
function gatePatterns(inc, kind) {
  if (inc && typeof inc === "object" && !(inc instanceof RegExp) && !Array.isArray(inc)) {
    inc = inc.include;
  }
  const list = Array.isArray(inc) ? inc : inc != null ? [inc] : [];
  const out = [];
  for (const r of list) {
    if (typeof r === "string") {
      out.push(kind === "id" ? { glob: idFilterGlob(r, initial.config?.root ?? process.cwd()) } : { literal: r });
    } else if (r instanceof RegExp) {
      out.push({ source: r.source, flags: r.flags });
    } else {
      return null;
    }
  }
  return out;
}
// The plugins of `list` with hook `name`, in the hook's order.
function hooksOf(list, name) {
  const out = [];
  for (const p of list) {
    const fn = hookFn(p, name);
    if (fn) out.push({ p, fn, rank: hookOrderRank(p[name]) });
  }
  out.sort((a, b) => a.rank - b.rank);
  return out;
}
function pluginsWithHook(c, name) {
  return hooksOf(c.plugins, name);
}

// Vite's pluginFilter: a string *id* filter is a picomatch glob (dot:true) joined
// to cwd unless it starts with `**` or is absolute, a RegExp is tested against the
// slash-normalized id (lastIndex reset), and a string *code* filter is a substring.
// The glob is matched with Vite's own picomatch (vite-globs.mjs).
const slash = (p) => p.replace(/\\/g, "/");
function idPatternToRegExp(pattern) {
  if (pattern instanceof RegExp) return pattern;
  return idFilterRegExp(pattern, initial.config?.root ?? process.cwd());
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
  if (
    ["js", "jsx", "ts", "tsx", "json", "css", "text", "base64", "dataurl", "binary", "empty", "asset"].includes(ext)
  ) {
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

function hookTransformMatches(hook, id, code, idOnly = false) {
  const f = hook && typeof hook === "object" ? hook.filter : null;
  if (!f) return true;
  if (!filterMatches(f.id, id, matchId)) return false;
  if (!idOnly && f.code != null && !filterMatches(f.code, code, matchCode)) return false;
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
async function resolveIdFull(c, source, importer, opts) {
  const options = {
    attributes: (opts && opts.attributes) || {},
    custom: (opts && opts.custom) || {},
    isEntry: !!(opts && opts.isEntry),
    ssr: c.environment.config?.consumer === "server",
    scan: !!(opts && opts.scan),
  };
  const skip = opts && opts.skip;
  const skipCalls = (opts && opts.skipCalls) || null;
  const phase = opts && opts.phase;
  for (const { p, fn: handler, rank } of withHtmlProxy(pluginsWithHook(c, "resolveId"), "resolveId", source)) {
    if (skip && p === skip) continue;
    if (phase && (phase === "pre") !== runsBeforeCoreResolver(p, rank)) continue;
    // Vite merges skipCalls into the skip set: a plugin is skipped when its
    // recorded call re-issues the same id + importer, or when it already
    // re-entered once (`called`) -- the recursion hard-stop.
    if (
      skipCalls &&
      skipCalls.some((c) => c.plugin === p && (c.called || (c.id === source && c.importer === importer)))
    )
      continue;
    if (!hookIdMatches(p.resolveId, source)) continue;
    let r;
    try {
      r = await handler.call(resolveHookCtx(c, p, skipCalls, options.scan), source, importer || undefined, options);
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
async function resolveId(c, source, importer, opts) {
  const r = await resolveIdFull(c, source, importer, opts);
  return r == null ? null : r.id;
}

// Vite's order around its core resolver (vite:resolve): a hook that runs
// before it (`enforce: "pre"`, or `order: "pre"`) sees every specifier first;
// the rest see only what the core resolver left. A Start bundle asks for one
// phase at a time, with its own resolution in between.
function runsBeforeCoreResolver(p, rank) {
  return rank < 0 || (rank === 0 && enforceRank(p) < 0);
}

// One phase ("pre", "post", or "all" for the whole chain) for a Start bundle:
// the winning result as JSON (`id` and, when set, `external` and
// `moduleSideEffects`), null when no plugin answered.
async function resolveIdPhase(c, source, importer, phase, isEntry) {
  const r = await resolveIdFull(c, source, importer || undefined, {
    phase: phase === "all" ? null : phase,
    isEntry: isEntry === "true",
  });
  if (r == null) return null;
  const out = { id: String(r.id) };
  if (r.external !== undefined && r.external !== false) out.external = r.external;
  if (typeof r.moduleSideEffects === "boolean") out.moduleSideEffects = r.moduleSideEffects;
  return JSON.stringify(out);
}

// The live config values a Start bundle reads (Vite reads them off its
// resolved config): `resolve.alias`, the client `external`, the chunk policy,
// the Cloudflare Worker environment, `server.hmr`/`server.watch`, and whether
// the React plugin is configured. A function or RegExp cannot cross to Rust
// as JSON, so a RegExp travels as `{ $regex, flags }` and a function as
// `{ $fn: n }`, a handle `callStartFn` invokes.
// The handles index the container's `startFns`, which `fns` is.
function factValue(v, fns, seen = new Set()) {
  if (v instanceof RegExp) return { $regex: v.source, flags: v.flags };
  if (typeof v === "function") return { $fn: fns.push(v) - 1 };
  if (Array.isArray(v)) return v.map((x) => factValue(x, fns, seen));
  if (v && typeof v === "object") {
    if (seen.has(v)) return null;
    seen.add(v);
    const out = {};
    for (const [k, x] of Object.entries(v)) if (x !== undefined) out[k] = factValue(x, fns, seen);
    return out;
  }
  return v;
}
async function startFacts(c) {
  const fns = (c.startFns = []);
  const cfg = c.ojConfig ?? c.resolvedConfig ?? {};
  const envOf = (name) => cfg.environments?.[name] ?? {};
  const self = envOf(c.environment.name);
  const top = cfg.build?.rolldownOptions ?? cfg.build?.rollupOptions ?? {};
  const own = self.build?.rolldownOptions ?? {};
  const output = { ...(top.output ?? {}), ...(own.output ?? {}) };
  const codeSplitting = output.codeSplitting ?? output.advancedChunks;
  const chunkOptimization = { ...(top.experimental ?? {}), ...(own.experimental ?? {}) }.chunkOptimization;
  const external = own.external ?? cfg.build?.rolldownOptions?.external ?? cfg.build?.rollupOptions?.external;
  const names = c.rawPlugins.map((p) => p && p.name);
  return JSON.stringify({
    alias: factValue(cfg.resolve?.alias ?? userResolveAlias ?? null, fns),
    external: external === undefined ? null : factValue(external, fns),
    codeSplitting: codeSplitting === undefined ? null : factValue(codeSplitting, fns),
    chunkOptimization: chunkOptimization === undefined ? null : factValue(chunkOptimization, fns),
    preserveEntrySignatures: own.preserveEntrySignatures ?? top.preserveEntrySignatures ?? null,
    serverHmr: cfg.server?.hmr === false ? false : true,
    serverWatch: cfg.server?.watch === null ? null : factValue(cfg.server?.watch ?? {}, fns),
    reactRefresh: names.includes("vite:react-refresh"),
    cloudflare: await cloudflareEnvironmentFacts(cfg, fns),
    worker: workerFacts(cfg, fns),
    // How many resolveId hooks run on each side of the core resolver.
    resolveIdPhases: pluginsWithHook(c, "resolveId").reduce(
      (n, { p, rank }) => {
        n[runsBeforeCoreResolver(p, rank) ? "pre" : "post"]++;
        return n;
      },
      { pre: 0, post: 0 },
    ),
  });
}
const stringOr = (v) => (typeof v === "string" ? v : null);
// Vite's resolved worker options: `format` (iife by default) and
// `rolldownOptions` (`rollupOptions` its older name; the first output of an
// array), which a worker bundle takes in place of the app's build options.
function workerFacts(c, fns) {
  const w = c.worker ?? userViteConfig?.worker ?? {};
  const options = w.rolldownOptions ?? w.rollupOptions ?? {};
  const output = (Array.isArray(options.output) ? options.output[0] : options.output) ?? {};
  return {
    format: stringOr(w.format) ?? "iife",
    external: options.external === undefined ? null : factValue(options.external, fns),
    entryFileNames: stringOr(output.entryFileNames),
    chunkFileNames: stringOr(output.chunkFileNames),
    assetFileNames: stringOr(output.assetFileNames),
  };
}
// @cloudflare/vite-plugin's Worker environment (its `config` hook declares
// one per Worker, `resolve.builtins` naming the runtime's modules), or null.
// With several Workers the entry Worker (the one writing the manifest) wins.
async function cloudflareEnvironmentFacts(c, fns) {
  const environments = c.environments;
  if (!environments || typeof environments !== "object") return null;
  const workers = Object.entries(environments).filter(([name, options]) => {
    if (name === "client" || !options) return false;
    const builtins = options.resolve?.builtins;
    return (
      (Array.isArray(builtins) && builtins.includes("cloudflare:workers")) ||
      options.build?.rolldownOptions?.input?.index === "virtual:cloudflare/worker-entry"
    );
  });
  if (!workers.length) return null;
  const [name, options] = workers.find(([, o]) => o.build?.manifest === true) ?? workers[0];
  const builtins = (options.resolve?.builtins ?? []).filter((b) => typeof b === "string" || b instanceof RegExp);
  return {
    name,
    builtins: factValue(builtins, fns),
    conditions: Array.isArray(options.resolve?.conditions)
      ? options.resolve.conditions.filter((x) => typeof x === "string" && !x.includes("|"))
      : ["workerd", "worker", "module", "browser"],
    // The environment's own main fields (Vite never lets the top-level list,
    // the client's, reach another environment).
    mainFields: Array.isArray(options.resolve?.mainFields)
      ? options.resolve.mainFields.filter((x) => typeof x === "string")
      : null,
    target: typeof options.build?.target === "string" ? options.build.target : null,
    // Rolldown builtin plugins the environment attached (Vite's
    // esmExternalRequirePlugin): their names and options, read as Vite's
    // build reads them (asyncFlatten(arraify(..))).
    rolldownPlugins: (await flattenPlugins(options.build?.rolldownOptions?.plugins)).map((p) => ({
      name: p.name ?? null,
      options: factValue(p._options ?? null, fns),
    })),
    // Where Vite puts the Worker: the plugin's config hook derives it from
    // the user's own config (`environments.<name>.build.outDir`, else
    // `<build.outDir>/<name>`), which a resolved base here already fills with
    // Vite's defaults; read the user's config as the plugin would.
    outDir: stringOr(userViteConfig?.environments?.[name]?.build?.outDir),
    rootOutDir: stringOr(userViteConfig?.build?.outDir),
  };
}
// A config function a fact handed out (`$fn`), called with JSON arguments
// (an `external` function: `(id, importer, isResolved)`).
async function callStartFn(c, index, argsJson) {
  const fn = c.startFns[Number(index)];
  if (typeof fn !== "function") throw new Error(`oj: no config function #${index}`);
  const r = await fn(...JSON.parse(argsJson ?? "[]"));
  return JSON.stringify(r === undefined ? null : factValue(r, c.startFns));
}
// A codeSplitting group's `name` or `test` over a batch of module ids, each
// called as rolldown calls it: `(id, { getModuleInfo })`. The module infos of
// a chunking pass arrive once, with its first batch (`generation` names the
// pass), and back `getModuleInfo` for every later batch of the same pass.
// A throw in dev comes back as `{ $threw: message }` for that id: Vite's dev
// server never evaluates these functions, so one written for the build (fed
// by a build-only plugin) must not take the dev server down.
async function callStartFnBatch(c, index, idsJson, generation, infosJson, tolerant) {
  const fn = c.startFns[Number(index)];
  if (typeof fn !== "function") throw new Error(`oj: no config function #${index}`);
  if (c.chunkInfos.generation !== generation) c.chunkInfos = { generation, byId: new Map() };
  const { byId } = c.chunkInfos;
  if (infosJson) for (const info of JSON.parse(infosJson)) byId.set(info.id, info);
  const ctx = { getModuleInfo: (id) => byId.get(id) ?? null };
  const out = [];
  for (const id of JSON.parse(idsJson)) {
    try {
      const r = await fn(id, ctx);
      out.push(r === undefined ? null : r);
    } catch (e) {
      if (tolerant !== "true") throw e;
      out.push({ $threw: String((e && e.message) || e) });
    }
  }
  return JSON.stringify(out);
}

// The dep optimizer's scan runs on oj's rolldown; it resolves each import
// through the plugins here with `scan: true` (Vite's scan goes through its
// plugin container) and runs the config's live
// optimizeDeps.rolldownOptions.plugins, which Vite puts in the scan's plugin
// list, through scanPluginHook.
async function scanResolveId(source, importer) {
  const r = await resolveIdFull(main, source, importer || undefined, { scan: true });
  return r == null ? null : JSON.stringify({ id: r.id, external: r.external ?? false });
}
let scanPlugins = null;
async function scanPluginList() {
  scanPlugins ??= flattenPlugins(main.resolvedConfig?.optimizeDeps?.rolldownOptions?.plugins);
  return scanPlugins;
}
async function scanPluginHooks() {
  const list = await scanPluginList();
  if (!list.length) return null;
  const has = (name) => list.some((p) => hookFn(p, name));
  return JSON.stringify({ resolveId: has("resolveId"), load: has("load"), transform: has("transform") });
}
async function scanPluginHook(hook, argsJson) {
  const args = JSON.parse(argsJson);
  const original = args[0];
  for (const p of await scanPluginList()) {
    const fn = hookFn(p, hook);
    if (!fn) continue;
    // Under Vite these plugins run in the scan's own rolldown build: a
    // `this.resolve` from any of their hooks goes through vite:dep-scan:resolve,
    // which asks the app's plugins with `scan: true`.
    const ctx = resolveHookCtx(main, p, null, true);
    if (hook === "resolveId") {
      if (!hookIdMatches(p.resolveId, args[0])) continue;
      const r = await fn.call(ctx, args[0], args[1] ?? undefined, {
        attributes: {},
        custom: {},
        isEntry: !!args[2]?.isEntry,
        kind: args[2]?.kind,
        scan: true,
      });
      if (r == null || r === false) continue;
      if (typeof r === "string") return JSON.stringify({ id: r });
      if (r.id != null) return JSON.stringify({ id: r.id, external: r.external ?? false });
    } else if (hook === "load") {
      if (!hookIdMatches(p.load, args[0])) continue;
      const r = await fn.call(ctx, args[0]);
      if (r == null) continue;
      if (typeof r === "string") return JSON.stringify({ code: r });
      if (r.code != null) return JSON.stringify({ code: r.code, moduleType: r.moduleType });
    } else if (hook === "transform") {
      if (!hookTransformMatches(p.transform, args[1], args[0])) continue;
      const r = await fn.call(ctx, args[0], args[1]);
      const code = typeof r === "string" ? r : r?.code;
      if (code != null) args[0] = code;
    }
  }
  return hook === "transform" && args[0] !== original ? JSON.stringify({ code: args[0] }) : null;
}

// Vite's vite:html-inline-proxy, which loads the inline module scripts the dev
// HTML transform lifts out. It sits among Vite's core plugins, after every
// `enforce: "pre"` plugin; it joins a hook's chain only for an html-proxy id in
// dev, so the plugin list (and every hook gate derived from it) is unchanged.
const htmlProxy = createHtmlProxy(() => main.resolvedConfig?.root ?? initial.config?.root ?? process.cwd());
function withHtmlProxy(entries, hook, id) {
  if (env.command === "build" || !isHtmlProxyRE.test(id)) return entries;
  let at = 0;
  while (at < entries.length && (entries[at].rank < 0 || (entries[at].rank === 0 && entries[at].p.enforce === "pre")))
    at++;
  const entry = { p: htmlProxy.plugin, fn: htmlProxy.plugin[hook].handler, rank: 0 };
  return [...entries.slice(0, at), entry, ...entries.slice(at)];
}

// The load chain (Vite pluginContainer.load): `{ ssr }` options, the first
// non-null result returned whole (code, map, meta) and its meta folded into
// the module info. `phase` "pre": only the hooks that run before Vite's core
// plugins, which a Start bundle asks first for the assets its own loader owns
// (vite-imagetools' `x.webp?w=768` comes before vite:asset).
async function loadFull(c, id, phase) {
  const options = { ssr: c.environment.config?.consumer === "server" };
  for (const { p, fn: handler, rank } of withHtmlProxy(pluginsWithHook(c, "load"), "load", id)) {
    if (phase === "pre" && !runsBeforeCoreResolver(p, rank)) continue;
    if (!hookIdMatches(p.load, id)) continue;
    let r;
    try {
      r = await handler.call(ctxFor(c, p), id, options);
    } catch (e) {
      throw decoratePluginError(e, p, id);
    }
    if (r == null) continue;
    if (typeof r === "string") return { code: r };
    if (typeof r !== "object" || r.code == null) continue;
    updateModuleInfo(c, id, r);
    return r;
  }
  return null;
}
async function load(c, id, phase) {
  const r = await loadFull(c, id, phase);
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

async function handleHotUpdate(c, file, timestamp, type, modulesJson) {
  if (runnerOwns(c)) return null;
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
  for (const { p, fn } of hotUpdatePlugins(c, hmrContext.type)) {
    let r;
    try {
      r = await fn.call(ctxFor(c, p), hmrContext);
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

// Vite 6+ (handleHMRUpdate): a plugin's `hotUpdate`, with this.environment,
// in every environment; the legacy `handleHotUpdate` in its stead runs once,
// in the client environment's pass, and only for an `update`.
function hotUpdatePlugins(c, type) {
  const out = [];
  for (const p of c.plugins) {
    const legacy = p.hotUpdate == null && !c.startAfter && (type === undefined || type === "update");
    const hook = p.hotUpdate != null ? p.hotUpdate : legacy ? p.handleHotUpdate : null;
    const fn = hookHandler(hook);
    if (fn) out.push({ p, fn, rank: hookOrderRank(hook) });
  }
  out.sort((a, b) => a.rank - b.rank);
  return out;
}

// `ctxJson`: Vite's IndexHtmlTransformContext for this page. Dev (indexHtml
// middleware): `{ path: url, filename: <abs file>, originalUrl }` plus the dev
// server; build (html.ts): `{ path: "/" + rel, filename: <abs file>, bundle,
// chunk }`. A throwing hook fails the request / build under Vite (the error
// reaches the error middleware), so it is rethrown decorated, not swallowed.
// `phase` (a build): "pre" runs the `order: "pre"` hooks, which Vite's
// vite:build-html applies to the page source in its transform, before it
// reads the page's scripts (ctx `{ path, filename }`); "post" the normal and
// post ones, which it applies in generateBundle with the bundle and chunk.
// Without one, every hook in order.
async function transformIndexHtml(html, ctxJson, phase) {
  let htmlCtx = null;
  try {
    htmlCtx = ctxJson ? JSON.parse(ctxJson) : null;
  } catch {}
  if (!htmlCtx || typeof htmlCtx !== "object") htmlCtx = {};
  if (htmlCtx.path == null) htmlCtx.path = "/index.html";
  if (env.command !== "build") return devTransformIndexHtml(htmlCtx.path, html, htmlCtx.originalUrl, htmlCtx);
  if (htmlCtx.filename == null)
    htmlCtx.filename = pathResolve(
      main.resolvedConfig?.root ?? initial.config?.root ?? process.cwd(),
      htmlCtx.path.replace(/^\//, ""),
    );
  const [preHooks, normalHooks, postHooks] = resolveHtmlTransforms(main.plugins, bindHtmlHook);
  const hooks =
    phase === "pre"
      ? preHooks
      : phase === "post"
        ? [...normalHooks, ...postHooks]
        : [...preHooks, ...normalHooks, ...postHooks];
  // Vite reaches the page after buildStart (UnoCSS's resets what it scanned).
  if (hooks.length === 0) return html;
  await gateOnBuildStart(main);
  return applyHtmlTransforms(html, hooks, htmlCtx, htmlWarn);
}

// A plugin's transformIndexHtml handler with its plugin context, its error
// decorated with the plugin name like every other hook's.
function bindHtmlHook(p, handler) {
  return {
    async fn(html, ctx) {
      try {
        return await handler.call(ctxFor(main, p), html, ctx);
      } catch (e) {
        throw decoratePluginError(e, p, ctx.filename);
      }
    },
  };
}
const htmlWarn = (msg) => {
  const logger = main.resolvedConfig?.logger;
  if (logger && typeof logger.warn === "function") logger.warn(msg);
  else process.stderr.write(`${msg}\n`);
};
const htmlWarned = new Set();
const htmlWarnOnce = (msg) => {
  if (htmlWarned.has(msg)) return;
  htmlWarned.add(msg);
  htmlWarn(msg);
};

// Vite's createDevHtmlTransformFn: every dev page and `server.transformIndexHtml`.
// The dev client oj serves stands where Vite injects `/@vite/client`. `extra`
// carries what the Rust page server knows exactly: the file, and the env map
// and CSP nonce it derives from the same defines and config Vite reads.
// @stylexjs/unplugin's transformIndexHtml, for pages this host transforms:
// oj runs the plugin natively (the host never loads it), but in `full` dev
// mode its hook adds the runtime script and stylesheet link to every page,
// a custom server's `server.transformIndexHtml` included.
function stylexHtmlPlugin(base) {
  const standIn = nativePlugins.find((p) => p && p.name === "@stylexjs/unplugin");
  if (!standIn || (standIn.__ojStylexOptions?.devMode ?? "full") !== "full") return null;
  return {
    name: "@stylexjs/unplugin",
    enforce: "pre",
    transformIndexHtml() {
      return [
        {
          tag: "script",
          attrs: { type: "module", src: posix.join(base, "/@id/virtual:stylex:runtime") },
          injectTo: "head",
        },
        { tag: "link", attrs: { rel: "stylesheet", href: posix.join(base, "/virtual:stylex.css") }, injectTo: "head" },
      ];
    },
  };
}

async function devTransformIndexHtml(url, html, originalUrl, extra = {}) {
  const config = { ...(main.resolvedConfig ?? {}) };
  config.root = config.root ?? initial.config?.root ?? process.cwd();
  const base = config.base || "/";
  const stylex = stylexHtmlPlugin(base);
  return transformDevHtml({
    url,
    html,
    originalUrl,
    filename: extra.filename,
    server: devServer,
    config,
    // An `enforce: 'pre'` plugin: ahead of the app's in Vite's sorted list.
    plugins: stylex ? [stylex, ...main.plugins] : main.plugins,
    bind: bindHtmlHook,
    proxy: htmlProxy,
    moduleGraph,
    clientTags: ["/@oj/refresh-preamble.js", "/@oj/client.js"].map((src) => ({
      tag: "script",
      attrs: { type: "module", src: posix.join(base, src) },
      injectTo: "head-prepend",
    })),
    // oj's CSS pipeline lives in Rust; the direct-drive tests (plain node)
    // have none, and leave inline styles as written.
    compileStyle: ojEnginePost ? (id, code) => asyncRpc("htmlStyle", [id, code]) : undefined,
    env: extra.ojHtmlEnv ?? undefined,
    nonce: extra.ojCspNonce ?? undefined,
    warn: htmlWarn,
    warnOnce: htmlWarnOnce,
  });
}

// A throwing lifecycle hook (buildStart, buildEnd, renderStart, generateBundle,
// writeBundle, closeBundle) rejects the whole build in rolldown/Vite; surface it
// as `[plugin:name] message` and let the caller fail instead of logging on.
async function runHookOrThrow(c, p, fn, args) {
  try {
    return await fn.apply(ctxFor(c, p), args);
  } catch (e) {
    throw decoratePluginError(e, p);
  }
}

// `buildEnd(error?)`: Rollup hands the hook the error that failed the build
// (Vite's pluginContainer.close and rolldown both do), so a failed build still
// reaches every plugin's cleanup with the cause.
// Vite's EnvironmentPluginContainer.close: buildEnd, then closeBundle with
// what buildEnd threw, which the close then rejects with.
async function closeEnvironment(c) {
  if (runnerOwns(c)) return null;
  let buildEndError;
  try {
    await runLifecycle(c, "buildEnd", []);
  } catch (e) {
    buildEndError = e;
  }
  await runLifecycle(c, "closeBundle", [], buildEndError);
  if (buildEndError) throw buildEndError;
  return null;
}
async function runLifecycle(c, hook, args, closeError) {
  // Rollup runs closeBundle once per build; oj reaches it from rolldown's own
  // failure path and from its explicit call, so the second is a no-op.
  if (hook === "closeBundle") {
    if (c.closeBundleDone) return null;
    c.closeBundleDone = true;
  }
  let hookArgs = [];
  if (hook === "buildEnd" && args && args[0]) {
    const err = new Error(String(args[0]));
    err.code = "BUILD_FAILED";
    hookArgs = [err];
  }
  if (hook === "closeBundle" && closeError) hookArgs = [closeError];
  for (const { p, fn } of pluginsWithHook(c, hook)) {
    // Vite's EnvironmentPluginContainer.close: buildEnd by buildStart's rule.
    if (hook === "buildEnd" && !runsInDevEnvironment(c, p, "perEnvironmentStartEndDuringDev")) continue;
    await runHookOrThrow(c, p, fn, hookArgs);
  }
  return null;
}

async function runBuildStart(c) {
  const chunkBucket = [];
  // Rollup passes NormalizedInputOptions; plugins such as @crxjs read
  // `options.input` to decide whether to emit their root chunk, so it must be
  // present. Chunks emitted here are drained to Rust as rolldown build roots.
  const options = {
    input: c.resolvedConfig?.build?.rollupOptions?.input ?? "index.html",
  };
  // Vite's dev server starts every plugin on the client environment; another
  // environment's buildStart runs only the plugins that ask for it.
  await transformEmitStore.run(chunkBucket, async () => {
    for (const { p, fn } of pluginsWithHook(c, "buildStart")) {
      if (!runsInDevEnvironment(c, p, "perEnvironmentStartEndDuringDev")) continue;
      await runHookOrThrow(c, p, fn, [options]);
    }
  });
  return JSON.stringify({ emittedChunks: chunkBucket });
}

// Vite runs buildStart before ANY serving hook: its dev server awaits every
// environment container's buildStart() while initializing, so a plugin that
// computes state in buildStart() and serves it from load() (an i18n barrel
// compiler) never sees the load first. The old architecture had that ordering
// structurally — Rust awaited the boot host's buildStart before serving, and
// the FIFO ssr container awaited its own buildStart before declaring itself
// ready — but the in-process hosts answer hooks as concurrent engine jobs
// with no such gate, so a lazily spawned host's first load/transform could
// run before (or race) buildStart. `gateOnBuildStart` restores the guarantee
// at the hook entry: serving hooks wait for the ONE buildStart to SETTLE
// (rollup runs it once per build, hence the memo). Errors keep their old
// homes — an explicit buildStart caller gets the rejection (Rust fails the
// boot, or logs-and-skips on the Start path) while a gated hook proceeds past
// a failed buildStart and serves on, as the old world did after logging.
// Vite awaits the client's buildStart before the server listens; another
// environment's container starts on its own first use, whatever the client's
// start came to.
function ensureBuildStart(c) {
  if (c.buildStartOwner) return ensureBuildStart(c.buildStartOwner);
  if (runnerOwns(c)) return Promise.resolve();
  const run = () => runBuildStart(c);
  return (c.buildStartRun ??= c.startAfter ? ensureBuildStart(c.startAfter).then(run, run) : run());
}
async function gateOnBuildStart(c) {
  try {
    await ensureBuildStart(c);
  } catch (e) {
    if (!c.buildStartFailureLogged) {
      c.buildStartFailureLogged = true;
      process.stderr.write(`${OJ} plugin host: buildStart failed: ${(e && e.message) || e}\n`);
    }
  }
}

async function generateBundle(c, bundleJson, isWrite) {
  const bundle = JSON.parse(bundleJson || "{}");
  const outputOptions = c.environment.config?.build ?? {};
  // Without `build.emitAssets` (the default off the client) vite:asset's
  // generateBundle deletes the bundle's assets, an ssr-manifest.json and JS
  // source maps aside. A core plugin with an unordered hook, it runs after
  // every `order: "pre"` hook and the enforce:"pre" plugins' unordered ones:
  // what was emitted before it is gone, later emits stay.
  const emitAssets = environmentEmitAssets(c);
  let assetStep = false;
  const viteAsset = () => {
    if (assetStep || emitAssets) return;
    assetStep = true;
    for (const [file, out] of Object.entries(bundle)) {
      if (out && out.type === "asset" && !keptWithoutEmitAssets(file)) delete bundle[file];
    }
    const kept = c.emitted.filter((e) => keptWithoutEmitAssets(e.fileName));
    c.emitted.length = 0;
    c.emitted.push(...kept);
  };
  for (const { p, fn, rank } of pluginsWithHook(c, "generateBundle")) {
    if (rank > 0 || (rank === 0 && p.enforce !== "pre")) viteAsset();
    await runHookOrThrow(c, p, fn, [outputOptions, bundle, isWrite]);
  }
  viteAsset();
  return JSON.stringify(bundle);
}

async function renderChunk(c, code, chunkJson) {
  const chunk = JSON.parse(chunkJson || "{}");
  // Rollup passes NormalizedOutputOptions as the 3rd arg; renderChunk hooks such
  // as UnoCSS's read `options.dir` (the resolved output dir) to key their
  // vite:css-post lookup, so provide it resolved against the root.
  const outDir = c.resolvedConfig?.build?.outDir ?? "dist";
  const root = c.resolvedConfig?.root ?? initial.config?.root ?? process.cwd();
  const options = { dir: pathResolve(root, outDir), format: "es" };
  let current = code;
  const maps = [];
  const mapPlugins = [];
  let mapBroken = null;
  for (const { p, fn } of pluginsWithHook(c, "renderChunk")) {
    const r = await fn.call(ctxFor(c, p), current, chunk, options);
    if (r == null) continue;
    if (typeof r === "string") {
      current = r;
      mapBroken ??= p.name ?? "?";
      continue;
    }
    if (r.code != null) current = r.code;
    if (r.map != null) {
      maps.push(typeof r.map === "string" ? r.map : JSON.stringify(r.map));
      mapPlugins.push(p.name ?? "?");
    } else if (r.code != null && r.map === undefined) mapBroken ??= p.name ?? "?";
  }
  return JSON.stringify({ code: current, maps, mapPlugins, mapBroken });
}

async function writeBundle(c, bundleJson, isWrite) {
  const bundle = JSON.parse(bundleJson || "{}");
  const outputOptions = c.environment.config?.build ?? {};
  for (const { p, fn } of pluginsWithHook(c, "writeBundle")) {
    await runHookOrThrow(c, p, fn, [outputOptions, bundle, isWrite]);
  }
  return null;
}

async function run(hook, args, c) {
  if (hook === "serverListening") {
    // oj's dev listener is bound (args = [port, interface address]): fire the
    // stub httpServer's "listening" — Vite's listen()-time signal.
    await ojAnnounceDevListener(Number(args[0]), args[1]);
    return null;
  }
  if (hook === "rpcReply") {
    settleRpc(Number(args[0]), JSON.parse(args[1] ?? "{}"));
    return null;
  }
  if (hook === "seedChunkNames") {
    try {
      const m = JSON.parse(args[0] ?? "{}");
      for (const [k, v] of Object.entries(m)) c.chunkFileNames.set(k, v);
    } catch {}
    return null;
  }
  if (hook === "openWorkerContainer") return String(await openWorkerContainer(c, JSON.parse(args[0] ?? "[]")));
  if (hook === "openEnvironmentContainer") return String(await openEnvironmentContainer(JSON.parse(args[0] ?? "{}")));
  if (hook === "closeContainer") {
    if (c !== main && (!c.durable || args[0] === "environment")) containers.delete(c.id);
    return null;
  }
  if (hook === "transform") return gateOnBuildStart(c).then(() => transform(c, args[0], args[1], args[2]));
  if (hook === "transformPhase")
    return gateOnBuildStart(c).then(() => transform(c, args[0], args[1], args[2], args[3]));
  if (hook === "resolveId") return gateOnBuildStart(c).then(() => resolveId(c, args[0], args[1]));
  if (hook === "resolveIdPhase")
    return gateOnBuildStart(c).then(() => resolveIdPhase(c, args[0], args[1], args[2], args[3]));
  if (hook === "startFacts") return startFacts(c);
  if (hook === "callStartFn") return callStartFn(c, args[0], args[1]);
  if (hook === "callStartFnBatch") return callStartFnBatch(c, args[0], args[1], args[2], args[3], args[4]);
  if (hook === "scanResolveId") return gateOnBuildStart(main).then(() => scanResolveId(args[0], args[1]));
  if (hook === "scanPluginHooks") return gateOnBuildStart(main).then(() => scanPluginHooks());
  if (hook === "scanPluginHook") return gateOnBuildStart(main).then(() => scanPluginHook(args[0], args[1]));
  if (hook === "load") return gateOnBuildStart(c).then(() => load(c, args[0]));
  if (hook === "loadPhase") return gateOnBuildStart(c).then(() => load(c, args[0], args[1]));
  if (hook === "handleHotUpdate") return handleHotUpdate(c, args[0], args[1], args[2], args[3]);
  if (hook === "transformIndexHtml") return gateOnBuildStart(main).then(() => transformIndexHtml(args[0], args[1]));
  if (hook === "transformIndexHtmlPhase") return transformIndexHtml(args[0], args[1], args[2]);
  if (hook === "buildStart") return ensureBuildStart(c);
  if (hook === "closeEnvironment") return closeEnvironment(c);
  if (hook === "buildEnd" || hook === "renderStart" || hook === "closeBundle") {
    return runLifecycle(c, hook, args);
  }
  if (hook === "wsConnection") return wsConnection();
  if (hook === "watchChange") return watchChange(c, args[0], args[1]);
  if (hook === "getEmittedFiles") {
    return JSON.stringify(c.emitted.map(({ fileName, source }) => ({ fileName, source })));
  }
  if (hook === "getPluginCss") return JSON.stringify(c.pluginCss);
  // Plugins wrap server.close() to dispose runtimes they started (@cloudflare/vite-plugin's workerd).
  if (hook === "closeDevServer") {
    await devServer?.close?.();
    return null;
  }
  if (hook === "warmEnvironments") return warmEnvironments();
  if (hook === "getWatchFiles") return JSON.stringify([...watchedFiles]);
  if (hook === "hasModuleParsed") return String(anyModuleParsed(c));
  if (hook === "bundleModuleParsed") return bundleModuleParsed(c, args[0]);
  if (hook === "switchEnvironment") return switchEnvironment(c, args[0]);
  if (hook === "buildApp") return buildApp(args[0]);
  if (hook === "replayModuleParsed") return replayModuleParsed(c, args[0]);
  if (hook === "hasTransformIndexHtml") {
    return String(c.plugins.some((p) => typeof htmlHookFn(p) === "function"));
  }
  if (hook === "getBuildHookPlan") {
    // Per-hook filter plan for the build's Rust-side gate: which plugins have
    // the hook, and each plugin's `filter` include patterns. Vite/rolldown push
    // hook filters into the bundler so a filtered hook is never called for a
    // module it does not claim; this hands oj build the same information. The
    // gate must only ever OVER-approximate, so anything it cannot represent
    // exactly makes that plugin "unfiltered" (always called): function-form
    // hooks, filters without includes (exclude- or moduleType-only), and
    // patterns gatePatterns cannot carry (Rust fails the ones it cannot
    // translate open on its side).
    const planFor = (name, withCode) => {
      // Outside a Start build the transform RPC is what fires moduleParsed,
      // so gating it would skip modules; any moduleParsed pins transform
      // unfiltered. Outside a bundle (oj's dev server), it also fills the
      // module-info map this.getModuleInfo()/getModuleIds() read, so a hook
      // that may call them later (the manifest-plugin shape) pins it too. A
      // bundle's getters read the bundle's own graph (bundleModuleInfo).
      const inBundle = command === "build" || c.bundle;
      if (
        name === "transform" &&
        withCode &&
        ((!bundleBuildOf(c) && anyModuleParsed(c)) ||
          (!inBundle &&
            (pluginsWithHook(c, "generateBundle").length > 0 ||
              pluginsWithHook(c, "renderChunk").length > 0 ||
              pluginsWithHook(c, "writeBundle").length > 0 ||
              pluginsWithHook(c, "buildEnd").length > 0)))
      ) {
        return { present: true, unfiltered: true, plugins: [] };
      }
      const entries = [];
      let unfiltered = false;
      let present = false;
      for (const p of c.plugins) {
        const h = p && p[name];
        if (typeof hookHandler(h) !== "function") continue;
        present = true;
        const f = h && typeof h === "object" ? h.filter : null;
        const id = f ? gatePatterns(f.id, "id") : null;
        // A normal or post transform's filter.code is tested on the code after
        // the vite:oxc strip (Vite's slot for it), which the gate never sees:
        // only its id patterns can gate.
        const afterStrip = withCode && viteTransformWithOxc && !runsBeforeOxcStrip(p, hookOrderRank(h));
        const code = withCode && f && !afterStrip ? gatePatterns(f.code, "code") : null;
        const idHas = Array.isArray(id) && id.length > 0;
        const codeHas = Array.isArray(code) && code.length > 0;
        // A pattern set gatePatterns refused (null) or a filter with no usable
        // includes cannot gate; that plugin must always be offered the module.
        if (
          (id === null && f && f.id != null) ||
          (withCode && !afterStrip && code === null && f && f.code != null) ||
          (!idHas && !codeHas)
        ) {
          unfiltered = true;
          continue;
        }
        entries.push({ id: idHas ? id : [], code: codeHas ? code : [] });
      }
      return { present, unfiltered, plugins: entries };
    };
    return JSON.stringify({
      transform: planFor("transform", true),
      load: planFor("load", false),
      resolveId: planFor("resolveId", false),
      watchChange:
        !runnerOwns(c) &&
        pluginsWithHook(c, "watchChange").some(({ p }) =>
          runsInDevEnvironment(c, p, "perEnvironmentWatchChangeDuringDev"),
        ),
      hotUpdate: !runnerOwns(c) && hotUpdatePlugins(c).length > 0,
    });
  }
  if (hook === "hasGenerateBundle") {
    // Without emitAssets vite:asset's own generateBundle runs.
    return String(pluginsWithHook(c, "generateBundle").length > 0 || (bundleBuildOf(c) && !environmentEmitAssets(c)));
  }
  if (hook === "generateBundle") return generateBundle(c, args[0], args[1] === "true");
  if (hook === "hasRenderChunk") {
    return String(pluginsWithHook(c, "renderChunk").length > 0);
  }
  if (hook === "renderChunk") return renderChunk(c, args[0], args[1]);
  if (hook === "hasWriteBundle") {
    return String(pluginsWithHook(c, "writeBundle").length > 0);
  }
  if (hook === "writeBundle") return writeBundle(c, args[0], args[1] === "true");
  if (hook === "getPluginCount") return String(c.plugins.length);
  if (hook === "getTopLevelPluginCount") return String(c.topLevelPlugins.length);
  if (hook === "getPluginNames") return JSON.stringify(c.plugins.map((p) => p.name ?? ""));
  if (hook === "getPluginConfig") {
    // JSON-safe subset of what config() hooks returned (functions/RegExps
    // drop), plus THIS environment's `environments.<name>.define` from the
    // resolved config: only the resolved config knows the per-environment
    // define of a vite-format app (the extraction does not emit
    // environments), and Vite layers it over the shared define.
    let define = null;
    try {
      const merged = {
        ...((c.configDelta ?? pluginConfigDelta).define ?? {}),
        ...((c.ojConfig ?? c.resolvedConfig)?.environments?.[c.environment.name]?.define ?? {}),
      };
      define = Object.keys(merged).length ? JSON.parse(JSON.stringify(merged)) : null;
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
    for (const p of c.plugins) {
      const t = p && p.transform;
      const f = t && typeof t === "object" ? t.filter : null;
      if (f && f.code != null) pats.push(...(gatePatterns(f.code, "code") ?? [{ any: true }]));
    }
    return JSON.stringify(pats);
  }
  if (hook === "getHasTransform") {
    return String(pluginsWithHook(c, "transform").length > 0);
  }
  if (hook === "getDepLoadFilters") {
    // Object-form `load` hooks' `filter.id` include patterns: a node_modules
    // module is offered to plugin load only when one matches its path.
    const pats = [];
    for (const p of c.plugins) {
      const l = p && p.load;
      const f = l && typeof l === "object" ? l.filter : null;
      if (f && f.id != null) pats.push(...(gatePatterns(f.id, "id") ?? [{ any: true }]));
    }
    return JSON.stringify(pats);
  }
  if (hook === "getResolveIdFilters") {
    // Object-form `resolveId` hooks' `filter.id` include patterns. Vite offers
    // every import (relative and absolute too) to plugin resolveId before its
    // own resolver; oj resolves natively first and routes a non-bare import
    // through the plugins only when one of these patterns claims it. A filter
    // with no include (exclude-only) claims nothing, like a function-form
    // hook: both stay plugin fallbacks for what oj cannot resolve, since
    // claiming every import would route the whole graph through /@id/.
    const pats = [];
    for (const p of c.plugins) {
      const r = p && p.resolveId;
      const f = r && typeof r === "object" ? r.filter : null;
      if (f && f.id != null) pats.push(...(gatePatterns(f.id, "id") ?? [{ any: true }]));
    }
    return JSON.stringify(pats);
  }
  if (hook === "getHasResolveId") {
    const has = c.plugins.some((p) => typeof hookHandler(p.resolveId) === "function");
    return String(has);
  }
  if (hook === "getHasLoad") {
    const has = c.plugins.some((p) => typeof hookHandler(p.load) === "function");
    return String(has);
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

// The hook entry points for the in-process engine: oj calls
// `ojRun(hook, args)` per hook (the reply is the call's own return value — no
// ids, no reply frames), and `ojHostReady()` is the boot trigger whose call
// evaluates this module's top level; its resolution (or rejection: a top-level
// throw that used to kill the node child) is the boot task's error carrier.
export async function ojRun(hook, args, container) {
  try {
    return await run(hook, args ?? [], containerOf(container));
  } catch (e) {
    // The same string the stdio protocol carried in its error frames.
    throw new Error(String((e && e.stack) || e));
  }
}
export function ojHostReady() {
  return true;
}
// Liveness probe for calls waiting out a long hook: a parked event loop
// answers instantly, so no answer means the isolate is wedged in synchronous
// JS or native code (the Rust side then declares the host gone).
export function ojPing() {
  return true;
}

// Under plain `node` (the direct-drive unit tests) the stdio protocol stays:
// newline JSON over stdin, replies and control pushes on stdout.
if (!ojEngineBoot) {
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
      settleRpc(msg.rpcReply, msg);
      return;
    }
    const { id, hook, args, container } = msg;
    try {
      const result = await run(hook, args ?? [], containerOf(container));
      ctl({ id, result: result ?? null });
    } catch (e) {
      ctl({ id, error: String((e && e.stack) || e) });
    }
  });
}

// The unconditional init-complete signal, in BOTH modes: the hook entry point
// is callable, so from here every hang is a hook's, not initialization's.
// Serve mode also pushes { ojServeInfo } (state-bearing); build mode has no
// push at all, so without this a hanging first hook would wait out Rust's
// whole init deadline blamed on initialization instead of failing on the
// per-call timeout. Rust treats ojInit, ojServeInfo, or the first reply —
// whichever lands first — as initialized.
ojInitDone = true;
ctl({ ojInit: true });
