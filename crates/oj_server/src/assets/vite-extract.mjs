// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { createRequire, isBuiltin, syncBuiltinESMExports } from "node:module";
import { pathToFileURL, fileURLToPath } from "node:url";
import { dirname, isAbsolute, resolve, sep } from "node:path";
import fs, { writeFileSync, readFileSync, existsSync, unlinkSync } from "node:fs";
import { format } from "node:util";

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

// Extraction inputs, assigned by `extract()` below. The extraction engine is
// short-lived (one isolate per extraction), so per-run state lives safely at
// module scope; a direct import for unit tests runs no side effects.
let configPath;
let appRoot;
let command;
let mode;
// false when the mode is only the command's default, not a CLI `--mode`, so a
// `mode` named by the config file may win (Vite: inlineConfig.mode || config.mode).
let modeExplicit;
// oj's cache dir for this app, excluded from the read recorder below.
let ojCacheDir = null;

// Everything extraction (Vite's own notices, oj's "not applied" warnings, the
// config's plugin prints) would have written to stderr. The result is a
// RETURNED value now, so a config that prints garbage can no longer corrupt
// the result channel; the transcript travels back as `__stderr` for the Rust
// side to print and replay on cache hits. Outside `extract()` (a direct unit
// test import) the sink is the real stderr.
let emitStderr = (text) => process.stderr.write(text);

function installOutputCapture(chunks) {
  const capture = (text) => {
    chunks.push(text);
  };
  emitStderr = capture;
  const asText = (chunk) => (typeof chunk === "string" ? chunk : new TextDecoder().decode(chunk));
  const wrapWrite = (stream, sink) => {
    try {
      stream.write = (chunk, ...rest) => {
        sink(asText(chunk));
        const cb = rest.find((a) => typeof a === "function");
        if (cb) cb();
        return true;
      };
    } catch {}
  };
  // stderr joins the transcript; stdout is swallowed, as the old subprocess's
  // captured-then-dropped stdout was. Deno's console writes past process.std*
  // straight to the fds, so the console methods are wrapped too.
  wrapWrite(process.stderr, capture);
  wrapWrite(process.stdout, () => {});
  try {
    const fmt = (args) => {
      try {
        return format(...args);
      } catch {
        return args.map(String).join(" ");
      }
    };
    for (const name of ["error", "warn"]) console[name] = (...args) => capture(fmt(args) + "\n");
    for (const name of ["log", "info", "debug", "trace"]) console[name] = () => {};
  } catch {}
}

// Extraction used to run in its own node child whose env writes and cwd died
// with it. In-process, shadow `process.env` with a plain copy (config code
// sees, and mutates, a private environment seeded from the real one — Vite's
// NODE_ENV dance and loadEnv writes never leak into oj or a parallel
// extraction) and pin `process.cwd()` to the app root, where the old child
// ran. fs calls on relative paths still resolve against oj's real cwd.
function isolateProcessForExtraction() {
  try {
    Object.defineProperty(process, "env", {
      value: { ...process.env },
      configurable: true,
      writable: true,
    });
  } catch {}
  try {
    process.cwd = () => appRoot;
  } catch {}
}

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
// the identical default itself from the defaultNodeEnv oj passes. (The twin
// logic in plugin-host.mjs still runs at module scope in its own node
// process; here it runs against the isolated process.env above.)
let defaultNodeEnv;
let nodeEnvWasSet;
const unsetOjNodeEnvForResolve = () => {
  if (!nodeEnvWasSet && process.env.NODE_ENV === defaultNodeEnv) delete process.env.NODE_ENV;
};

// Config-shaped files the config evaluation touched. The Cloudflare plugin's
// `config` hook reads the Worker config (wrangler.unstable_readConfig) from
// wherever `cloudflare({ configPath })` and `auxiliaryWorkers[].configPath`
// point — under ANY filename (a custom worker.jsonc, a
// CLOUDFLARE_VITE_WRANGLER_CONFIG_PATH override, a
// .wrangler/deploy/config.json redirect) — files the root-level cache epoch
// (wrangler.* in the app root) cannot see. Env files count too: Vite's
// resolveConfig loads `.env`, `.env.local`, `.env.${mode}` and
// `.env.${mode}.local` under the RESOLVED envDir (getEnvFilesForMode →
// loadEnv), and their values change the verdict — a mode- or
// envDir-addressed file the root-level epoch cannot name either. Recording
// the actual reads is the honest signal (no config-shape or filename
// heuristics): every read of a .json/.jsonc/.toml or .env* file observed
// during the evaluation, outside node_modules and oj's own cache dir, joins
// `__deps`, so the Rust extraction cache stamps it like a config import.
// Attempted reads and existence probes of MISSING files are recorded too —
// the absent state is stamped, so creating the file later is a cache miss.
// Growth is guarded: reads during config evaluation are few, the set dedups,
// and recording stops at a cap — flagging the overflow (`__depsTruncated`)
// so the Rust side serves the result WITHOUT caching it, an incomplete stamp
// being worse than no cache entry.
const observedConfigReads = new Set();
let observedReadsTruncated = false;
const OBSERVED_READS_MAX = Number(process.env.OJ_OBSERVED_READS_MAX) || 512;
let recorderInstalled = false;
function installConfigReadRecorder() {
  if (recorderInstalled) return;
  recorderInstalled = true;
  const CONFIG_FILE = /\.(?:json|jsonc|toml)$/i;
  const ENV_FILE = /(?:^|[\\/])\.env(?:\.[^\\/]*)?$/;
  const record = (p) => {
    try {
      const s = typeof p === "string" ? p : p instanceof URL ? fileURLToPath(p) : null;
      if (!s || !(CONFIG_FILE.test(s) || ENV_FILE.test(s))) return;
      // Exclusions run on the RESOLVED path: resolve() also normalizes the
      // separators, so a forward-slash path on win32 cannot dodge the
      // node_modules check.
      const abs = resolve(appRoot, s);
      if (abs.split(sep).includes("node_modules") || abs.split(sep).includes(".oj-cache")) return;
      if (ojCacheDir && (abs === ojCacheDir || abs.startsWith(ojCacheDir + sep))) return;
      if (observedConfigReads.size >= OBSERVED_READS_MAX) {
        // Only a genuinely NEW path past the cap makes the stamp incomplete.
        if (!observedConfigReads.has(abs)) observedReadsTruncated = true;
        return;
      }
      observedConfigReads.add(abs);
    } catch {}
  };
  const wrap = (obj, name) => {
    const orig = obj[name];
    if (typeof orig !== "function") return;
    obj[name] = function (p, ...rest) {
      record(p);
      return orig.call(this, p, ...rest);
    };
  };
  // The core fs singleton: patched before any plugin code loads, so CJS
  // require("fs") consumers (wrangler) and ESM default-import consumers see
  // the wrappers. Under the embedded engine syncBuiltinESMExports is a no-op
  // (Deno's node:fs named bindings are the polyfill's own, not the default
  // object's properties), so `import { readFileSync } from "node:fs"`
  // consumers are NOT observed here — recordEnvFilesForMode below stamps the
  // env files Vite loads that way deterministically instead.
  for (const name of ["readFileSync", "readFile", "existsSync", "statSync"]) wrap(fs, name);
  wrap(fs.promises, "readFile");
  wrap(fs.promises, "stat");
  syncBuiltinESMExports();
}

// The env files Vite's resolveConfig loads for the resolved mode and envDir
// (getEnvFilesForMode -> loadEnv), stamped whether present or absent: their
// values change the extracted verdict, and Vite reads them through named fs
// imports the recorder cannot observe under the embedded engine. A fixed set
// of four known paths, so they join the stamp directly, outside the
// recorder's overflow accounting.
function recordEnvFilesForMode(resolvedMode, envDir) {
  const dir = resolve(appRoot, envDir || ".");
  for (const name of [".env", ".env.local", `.env.${resolvedMode}`, `.env.${resolvedMode}.local`]) {
    observedConfigReads.add(resolve(dir, name));
  }
}

let appRequire;
let directDeps = [];
function resolvePkg(spec) {
  try {
    return appRequire.resolve(spec);
  } catch {}
  for (const anchor of directDeps) {
    let dir;
    try {
      dir = dirname(appRequire.resolve(anchor + "/package.json"));
    } catch {
      continue;
    }
    try {
      return appRequire.resolve(spec, { paths: [dir] });
    } catch {}
  }
  return null;
}

const absDeps = (deps) =>
  (deps ?? []).filter((d) => typeof d === "string").map((d) => resolve(appRoot, d));

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

// A plugin appended LAST to the config handed to resolveConfig (enforce
// "post", both hooks `order: "post"`): Vite's own hook pipeline runs it at the
// very end of the config phase (getSortedPluginsByHook makes hook.order
// primary), so it observes the fully hook-merged config BEFORE Vite
// default-fills every environment's dev.createEnvironment
// (resolveDevEnvironmentOptions runs only after runConfigEnvironmentHook).
// That makes runner-backed detection resolveConfig's OWN single hook run —
// no re-run on the same plugin instances, no skip lists needed here — and it
// sees config-hook declarations (via the merged config.environments) and
// configEnvironment declarations (per environment, post-merge) alike.
// `fileBuildSsr`: the config FILE's own `build.ssr`. The single-eval flow
// hands the file's config to resolveConfig as the INLINE config
// (configFile: false), but Vite computes `configEnv.isSsrBuild` from the
// inline config alone (node.js resolveConfig: `isSsrBuild: command === "build"
// && !!config.build?.ssr`, before loadConfigFromFile) — so a file-set
// build.ssr must NOT ride the inline copy, or hooks see isSsrBuild true where
// real Vite gives false. Under real Vite the value re-enters the config right
// after the file loads (`config = mergeConfig(loadResult.config, config)`),
// BEFORE the config hooks run — the pre plugin below reattaches it at exactly
// that seam (runConfigHook, order "pre", first), so hooks, the environments.ssr
// default fill, and the base resolution all see it as they would under Vite.
// The one remaining divergence: a plugin's `apply(config, env)` filter runs
// before any hook and sees no build.ssr.
function detectionSentinel(fileBuildSsr) {
  let declared = false;
  let started = false;
  const sniff = (cfg) => {
    if (declaresRunnerEnvironment(cfg)) declared = true;
  };
  return {
    // Ordered FIRST (enforce "pre" + hook order "pre", prepended): marks that
    // resolveConfig's hook run began on these plugin instances — after this, a
    // resolveConfig failure must never re-run the hooks out of band — and
    // reattaches the file's build.ssr (above).
    prePlugin: {
      name: "oj:extraction-preamble",
      enforce: "pre",
      config: {
        order: "pre",
        handler() {
          started = true;
          if (fileBuildSsr !== undefined) return { build: { ssr: fileBuildSsr } };
        },
      },
    },
    plugin: {
      name: "oj:runner-environment-detection",
      enforce: "post",
      config: { order: "post", handler: sniff },
      configEnvironment: {
        order: "post",
        handler(name, envConfig) {
          if (envConfig && envConfig.dev && typeof envConfig.dev.createEnvironment === "function") {
            declared = true;
          }
        },
      },
    },
    declared: () => declared,
    // Whether resolveConfig got as far as running config hooks: a throw after
    // this point leaves side effects behind (run-once guards fired), so the
    // caller must consume the partial verdict instead of re-running hooks.
    started: () => started,
  };
}

// A deep copy of the config's PLAIN data (object literals and arrays), taken
// before resolveConfig runs any plugin hook: everything the emit later reads
// from the raw config (the resolve block for rawResolve, the ssr block, the
// scalars and shapes warnUnsupported inspects) must reflect the FILE's own
// content, not what a hook mutated in place. Functions, RegExps and class
// instances are kept by reference — warnUnsupported's `instanceof RegExp` and
// `typeof === "function"` checks must still see the real values, and plugin
// instances are not data to copy.
function snapshotPlainData(v, seen = new WeakMap()) {
  if (!v || typeof v !== "object") return v;
  const cached = seen.get(v);
  if (cached) return cached;
  if (Array.isArray(v)) {
    const out = [];
    seen.set(v, out);
    for (const x of v) out.push(snapshotPlainData(x, seen));
    return out;
  }
  const proto = Object.getPrototypeOf(v);
  if (proto !== Object.prototype && proto !== null) return v;
  const out = {};
  seen.set(v, out);
  for (const [k, x] of Object.entries(v)) out[k] = snapshotPlainData(x, seen);
  return out;
}

async function loadConfig() {
  let viteErr = null;
  const vitePath = resolvePkg("vite");
  if (vitePath) {
    try {
      const vite = await import(pathToFileURL(vitePath).href);
      // Vite evaluates the config file ONCE: resolveConfig itself calls
      // loadConfigFromFile and merges the inline config over the file's
      // (node.js resolveConfig: `config = mergeConfig(loadResult.config,
      // config)`). Mirror that single-eval flow — the file loaded first, its
      // config fed back through `configFile: false` — so the raw config and
      // the resolved one come from one evaluation. The old shape (resolveConfig
      // from the file, then loadConfigFromFile again for the raw copy) ran
      // module-level side effects twice, and the second load could fail alone,
      // leaving raw null and runner-backed detection silently false while the
      // resolved workerd sugar was still emitted.
      let loaded = null;
      let loadErr = null;
      if (typeof vite.loadConfigFromFile === "function") {
        try {
          // The configEnv Vite computes before the file loads: mode from the
          // inline config only, isSsrBuild from the inline config only (see
          // the emit below for the isSsrBuild rule).
          const configEnv = { command, mode, isSsrBuild: false };
          loaded = (await vite.loadConfigFromFile(configEnv, configPath, appRoot)) ?? null;
        } catch (e) {
          loadErr = e;
        }
      }
      // resolveConfig runs the plugins' config hooks, so plugin-injected values
      // (e.g. TanStack Start's resolve.alias for `#tanstack-router-entry`) are
      // present. loadConfigFromFile only reads the raw user config and misses them.
      if (typeof vite.resolveConfig === "function") {
        const singleEval = !!(loaded && loaded.config && typeof vite.mergeConfig === "function");
        // Hoisted OUT of the try: a resolveConfig throw mid-hook-run must not
        // discard the sentinel's partial verdict or the raw snapshot.
        let detect = null;
        let rawSnapshot = null;
        try {
          let resolved;
          if (singleEval) {
            // The plain-data subtrees the emit reads from the RAW config
            // (rawResolve, warnUnsupported), snapshotted BEFORE any hook runs:
            // mergeConfig shares untouched subobjects with loaded.config, so a
            // config hook that MUTATES the config in place (conditions.push)
            // would otherwise write into the object later emitted as raw.
            rawSnapshot = snapshotPlainData(loaded.config);
            detect = detectionSentinel(loaded.config.build?.ssr);
            const inline = modeExplicit ? { root: appRoot, mode } : { root: appRoot };
            const merged = vite.mergeConfig(loaded.config, inline);
            merged.configFile = false;
            // The file's build.ssr must not ride the inline config (isSsrBuild
            // is computed from it; see detectionSentinel) — the sentinel's pre
            // plugin reattaches the value at Vite's own post-load merge seam.
            if (merged.build && merged.build.ssr !== undefined) {
              merged.build = { ...merged.build };
              delete merged.build.ssr;
            }
            merged.plugins = [
              detect.prePlugin,
              ...(Array.isArray(merged.plugins) ? merged.plugins : []),
              detect.plugin,
            ];
            unsetOjNodeEnvForResolve();
            resolved = await vite.resolveConfig(merged, command, mode, defaultNodeEnv);
          } else {
            const inline = { root: appRoot, configFile: configPath };
            if (modeExplicit) inline.mode = mode;
            unsetOjNodeEnvForResolve();
            resolved = await vite.resolveConfig(inline, command, mode, defaultNodeEnv);
          }
          if (resolved) {
            // The raw user file, for the "not applied" warnings: the resolved
            // config carries Vite's defaults for every option (esbuild.jsxDev,
            // worker, ssr.resolve, cors.origin, terserOptions...), which are not
            // configuration oj is failing to honor.
            const raw = rawSnapshot ?? loaded?.config ?? null;
            if (raw == null) {
              // Loud, never silent: with no raw config, runner-backed detection
              // cannot run and the emit below withholds the resolved ssr sugar
              // and the resolved top-level conditions.
              warn(
                `could not load the raw config from ${configPath}` +
                  `${loadErr ? ` (${(loadErr && loadErr.message) || loadErr})` : ""}; ` +
                  "runner-backed detection and ssr.resolve conditions are unavailable",
              );
            }
            return {
              config: resolved,
              raw,
              deps: absDeps(singleEval ? loaded.dependencies : resolved.configFileDependencies),
              // Decided inside resolveConfig's own hook run; null when the
              // sentinel could not ride along (the caller falls back to
              // detectSsrRunnerBacked on the raw config's plugin instances,
              // whose hooks never ran in this process then).
              runnerBacked: detect ? detect.declared() : null,
              merge: typeof vite.mergeConfig === "function" ? vite.mergeConfig : null,
            };
          }
        } catch (e) {
          if (detect && detect.started()) {
            // resolveConfig threw AFTER plugin config hooks began running on
            // these instances (a throwing configResolved, a mid-run hook
            // failure). Re-running the hooks out of band would double every
            // side effect (run-once guards break), so consume the sentinel's
            // partial verdict instead — loudly, naming the failure.
            warn(
              `resolveConfig failed after plugin config hooks ran (${(e && e.message) || e}); ` +
                "using the raw config and the partial runner-backed verdict",
            );
            return {
              config: loaded.config,
              raw: rawSnapshot ?? loaded.config,
              deps: absDeps(loaded.dependencies),
              // The sentinel's verdict is partial: a hook throwing BEFORE the
              // post-ordered sniff ran leaves declared() false even when the
              // raw config itself declares a runner environment (the CF shape:
              // environments.<name>.dev.createEnvironment in the file). OR in
              // the shape-only raw check — it runs no hook, so no side effect
              // doubles — instead of emitting a bare false negative.
              runnerBacked: detect.declared() || declaresRunnerEnvironment(loaded.config),
              merge: typeof vite.mergeConfig === "function" ? vite.mergeConfig : null,
            };
          }
          // No hook ran in this process: fall through to the raw loader below
          // (detection there runs the hooks once, on instances still unrun).
        }
      }
      if (loaded && loaded.config) {
        return {
          config: loaded.config,
          raw: loaded.config,
          deps: absDeps(loaded.dependencies),
          merge: typeof vite.mergeConfig === "function" ? vite.mergeConfig : null,
        };
      }
    } catch (e) {
      viteErr = e;
    }
  }
  // The fallback loaders evaluate the config in THIS process: re-assert the
  // pre-load NODE_ENV (a resolveConfig attempt above may have unset it).
  if (!process.env.NODE_ENV) process.env.NODE_ENV = defaultNodeEnv;
  if (/\.(ts|tsx|mts|cts)$/.test(configPath)) {
    const esbuildPath = resolvePkg("esbuild");
    if (!esbuildPath) {
      throw viteErr ?? new Error("no vite or esbuild available to load the TS vite.config");
    }
    const esbuild = await import(pathToFileURL(esbuildPath).href);
    const r = await esbuild.build({
      entryPoints: [configPath], bundle: true, platform: "node", format: "esm",
      // Node's resolver, not esbuild's bundler defaults: `conditions` pins
      // "node" (import/require still apply per kind) and drops esbuild's
      // implicit "module" condition; `mainFields` skips "module" as Node does.
      conditions: ["node"], mainFields: ["main"],
      plugins: [externalizeDepsPlugin(), injectFileScopeVariablesPlugin()],
      write: false, logLevel: "silent", absWorkingDir: appRoot,
      metafile: true,
      define: CONFIG_BUNDLE_DEFINES,
    });
    // A unique path per process: the plugin host bundles the same config into
    // this directory concurrently at boot, and two writers on one filename made
    // either importer see a truncated bundle.
    const out = resolve(
      dirname(fileURLToPath(import.meta.url)),
      `oj-vite-config-${process.pid}-${Math.random().toString(36).slice(2)}.tmp.mjs`,
    );
    writeFileSync(out, r.outputFiles[0].text);
    let m;
    try {
      m = await import(pathToFileURL(out).href);
    } finally {
      try { unlinkSync(out); } catch {}
    }
    const config = typeof m.default === "function" ? await m.default({ command, mode }) : m.default;
    return { config, raw: config, deps: absDeps(Object.keys(r.metafile?.inputs ?? {})) };
  }
  const m = await import(pathToFileURL(configPath).href);
  const config = typeof m.default === "function" ? await m.default({ command, mode }) : m.default;
  return { config, raw: config, deps: relativeImportDeps(configPath) };
}

// A plain JS config loaded straight by Node has no bundler metafile to name its
// imports, so walk its relative `import` specifiers (Vite's
// configFileDependencies for the same file): the dev server restarts when one
// of those files changes, as it does for the config itself.
function relativeImportDeps(entry) {
  const seen = new Set();
  const stack = [entry];
  const spec = /(?:\bfrom\s*|\bimport\s*\(?\s*)["'](\.{1,2}\/[^"']+)["']/g;
  while (stack.length) {
    const file = stack.pop();
    if (seen.has(file)) continue;
    seen.add(file);
    let src;
    try { src = readFileSync(file, "utf8"); } catch { continue; }
    for (const m of src.matchAll(spec)) {
      const dep = resolve(dirname(file), m[1]);
      if (!seen.has(dep) && existsSync(dep)) stack.push(dep);
    }
  }
  seen.delete(entry);
  return [...seen];
}

function aliasKeyFromRegex(source) {
  let s = source;
  if (s.startsWith("^")) s = s.slice(1);
  if (s.endsWith("$")) s = s.slice(0, -1);
  s = s.replace(/\\?\/?\((?:\.\*|\.\+)\??\)\??$/, "");
  s = s.replace(/\\(.)/g, "$1");
  return s.replace(/\/$/, "");
}
function aliasDirFromReplacement(replacement) {
  return replacement
    .replace(/[\\/]\$\d+$/, "")
    .replace(/[\\/]index\.[a-z]+$/i, "");
}

const VITE_CLIENT_ALIAS = /^\^\\\/\?@vite\\\/(env|client)$/;

function extractAlias(alias) {
  const out = {};
  if (!alias) return out;
  const entries = Array.isArray(alias)
    ? alias.map((e) => [e.find, e.replacement])
    : Object.entries(alias);
  for (const [find, replacement] of entries) {
    if (typeof replacement !== "string") continue;
    if (typeof find === "string") {
      if (out[find] == null) out[find] = replacement;
    } else if (find instanceof RegExp) {
      // Vite adds these two for its own client (config.ts clientAlias); oj serves
      // /@vite/client and /@vite/env itself, so they are not user aliases to warn about.
      if (VITE_CLIENT_ALIAS.test(find.source)) continue;
      const key = aliasKeyFromRegex(find.source);
      if (!key || /[.*+?()[\]{}|^$]/.test(key)) {
        warn(`resolve.alias regex ${find} not convertible to a path alias; skipped`);
        continue;
      }
      if (out[key] == null) out[key] = aliasDirFromReplacement(replacement);
    }
  }
  return out;
}

function stringMap(obj) {
  if (!obj || typeof obj !== "object") return null;
  const out = {};
  for (const [k, v] of Object.entries(obj)) if (typeof v === "string") out[k] = v;
  return Object.keys(out).length ? out : null;
}

const warn = (msg) => emitStderr(`oj: vite.config: ${msg}\n`);

function extractProxy(proxy) {
  if (!proxy || typeof proxy !== "object") return null;
  const out = {};
  for (const [ctx, v] of Object.entries(proxy)) {
    if (typeof v === "string") {
      out[ctx] = v;
    } else if (v && typeof v === "object" && typeof v.target === "string") {
      const entry = { target: v.target };
      if (typeof v.changeOrigin === "boolean") entry.changeOrigin = v.changeOrigin;
      if (typeof v.ws === "boolean") entry.ws = v.ws;
      if (typeof v.secure === "boolean") entry.secure = v.secure;
      if (typeof v.rewriteWsOrigin === "boolean") entry.rewriteWsOrigin = v.rewriteWsOrigin;
      // A FUNCTION `rewrite` cannot cross this JSON config bridge, but it is no
      // longer dropped: when a plugin host runs it hosts the single
      // `server.proxy` from the app's real (un-serialized) config, where the
      // function is intact, for both the browser and the worker's outbound
      // fetch. Only a plain app with no plugin host still falls back to the
      // {from,to} form. `bypass` runs on the plugin host proxy; `configure`
      // needs http-proxy (bundled into Vite, usually not resolvable), so be
      // honest that it applies only when http-proxy is available.
      if (typeof v.bypass === "function") {
        warn(`server.proxy["${ctx}"].bypass is a function; applied by the plugin host proxy, not the built-in fallback proxy`);
      }
      if (typeof v.configure === "function") {
        warn(`server.proxy["${ctx}"].configure is a function; applied only when http-proxy is resolvable (the built-in fallback proxy cannot run it)`);
      }
      out[ctx] = entry;
    }
  }
  return Object.keys(out).length ? out : null;
}

function extractOptimizeDeps(od) {
  if (!od || typeof od !== "object") return null;
  const strArr = (v) =>
    typeof v === "string" ? [v] : Array.isArray(v) ? v.filter((x) => typeof x === "string") : undefined;
  const out = {};
  const inc = strArr(od.include);
  const exc = strArr(od.exclude);
  const ent = strArr(od.entries);
  const interop = strArr(od.needsInterop);
  if (inc) out.include = inc;
  if (exc) out.exclude = exc;
  if (ent) out.entries = ent;
  if (interop) out.needsInterop = interop;
  if (typeof od.force === "boolean") out.force = od.force;
  return Object.keys(out).length ? out : null;
}

// `build` values oj's build honors. `resolveConfig` returns Vite's defaults for
// anything the user left unset (outDir "dist", minify "oxc", sourcemap false,
// cssCodeSplit true, target "baseline-widely-available"); the boolean/string
// shapes oj adopts match its own defaults for those, and the special target
// names (which rolldown's transform does not parse) are skipped so oj's default
// lowering stands.
function extractBuild(b) {
  if (!b || typeof b !== "object") return null;
  const out = {};
  if (typeof b.outDir === "string") out.outDir = b.outDir;
  if (typeof b.sourcemap === "boolean" || typeof b.sourcemap === "string") out.sourcemap = b.sourcemap;
  if (typeof b.minify === "boolean" || typeof b.minify === "string") out.minify = b.minify;
  if (typeof b.cssCodeSplit === "boolean") out.cssCodeSplit = b.cssCodeSplit;
  if (typeof b.target === "string") out.target = b.target;
  else if (Array.isArray(b.target)) out.target = b.target.filter((t) => typeof t === "string");
  else if (b.target === false) warn("build.target false is not supported; the default baseline is used");
  if (typeof b.cssTarget === "string") out.cssTarget = b.cssTarget;
  else if (Array.isArray(b.cssTarget)) out.cssTarget = b.cssTarget.filter((t) => typeof t === "string");
  else if (b.cssTarget === false) warn("build.cssTarget false is not supported; build.target is used");
  if (typeof b.cssMinify === "boolean" || typeof b.cssMinify === "string") out.cssMinify = b.cssMinify;
  if (typeof b.emptyOutDir === "boolean") out.emptyOutDir = b.emptyOutDir;
  if (b.modulePreload === false) out.modulePreload = false;
  else if (b.modulePreload && typeof b.modulePreload === "object") {
    if (typeof b.modulePreload.resolveDependencies === "function") {
      warn("build.modulePreload.resolveDependencies is a function and cannot be applied");
    }
    if (typeof b.modulePreload.polyfill === "boolean") out.modulePreload = { polyfill: b.modulePreload.polyfill };
  }
  if (typeof b.ssr === "string" || typeof b.ssr === "boolean") out.ssr = b.ssr;
  if (typeof b.ssrManifest === "string" || typeof b.ssrManifest === "boolean") out.ssrManifest = b.ssrManifest;
  if (typeof b.copyPublicDir === "boolean") out.copyPublicDir = b.copyPublicDir;
  if (typeof b.manifest === "string" || typeof b.manifest === "boolean") out.manifest = b.manifest;
  if (typeof b.cssMinify === "boolean" || typeof b.cssMinify === "string") out.cssMinify = b.cssMinify;
  if (typeof b.assetsDir === "string") out.assetsDir = b.assetsDir;
  if (typeof b.reportCompressedSize === "boolean") out.reportCompressedSize = b.reportCompressedSize;
  if (typeof b.chunkSizeWarningLimit === "number") out.chunkSizeWarningLimit = b.chunkSizeWarningLimit;
  if (b.write === false) out.write = false;
  if (b.watch && typeof b.watch === "object") out.watch = {};
  if (b.license && b.license !== false) out.license = true;
  // Resolved defaults (cssTarget = target, commonjsOptions = { include:
  // [/node_modules/], extensions: [".js", ".cjs"] }) are not user choices.
  if (b.cssTarget !== undefined && JSON.stringify(b.cssTarget) !== JSON.stringify(b.target)) out.cssTarget = b.cssTarget;
  const cjs = b.commonjsOptions;
  if (cjs && typeof cjs === "object") {
    const extra = Object.keys(cjs).filter((k) => k !== "include" && k !== "extensions");
    const defaultInclude = Array.isArray(cjs.include) && cjs.include.length === 1 && String(cjs.include[0]) === "/node_modules/";
    const defaultExt = JSON.stringify(cjs.extensions) === JSON.stringify([".js", ".cjs"]);
    if (extra.length || !defaultInclude || !defaultExt) out.commonjsOptions = {};
  }
  if (b.lib && typeof b.lib === "object") {
    const lib = {};
    const e = b.lib.entry;
    const isStrList = (v) => Array.isArray(v) && v.every((s) => typeof s === "string");
    const isStrMap = (v) => v && typeof v === "object" && !Array.isArray(v) && Object.values(v).every((s) => typeof s === "string");
    if (typeof e === "string" || isStrList(e) || isStrMap(e)) lib.entry = e;
    if (typeof b.lib.name === "string") lib.name = b.lib.name;
    if (isStrList(b.lib.formats)) lib.formats = b.lib.formats;
    if (typeof b.lib.fileName === "string") lib.fileName = b.lib.fileName;
    else if (typeof b.lib.fileName === "function") warn("build.lib.fileName is a function and cannot be applied; the default file name is used");
    if (typeof b.lib.cssFileName === "string") lib.cssFileName = b.lib.cssFileName;
    if (lib.entry) out.lib = lib;
    else warn("build.lib.entry is required when build.lib is set");
  }
  return Object.keys(out).length ? out : null;
}

// rollup/rolldown options travel as JSON. A function (manualChunks, *FileNames,
// external) or RegExp cannot; mark them so oj warns instead of silently
// dropping the option.
function markFunctions(v) {
  if (typeof v === "function") return "__oj_fn__";
  if (v instanceof RegExp) return { __oj_regex__: v.source };
  if (Array.isArray(v)) return v.map(markFunctions);
  if (v && typeof v === "object") {
    const out = {};
    for (const [k, x] of Object.entries(v)) out[k] = markFunctions(x);
    return out;
  }
  return v;
}


const JSX_ESBUILD_KEYS = ["jsx", "jsxImportSource", "jsxFactory", "jsxFragment"];
function extractOxc(oxc) {
  const jsx = oxc && typeof oxc === "object" ? oxc.jsx : null;
  if (!jsx || typeof jsx !== "object") return null;
  const out = {};
  for (const k of ["runtime", "importSource", "pragma", "pragmaFrag"]) {
    if (typeof jsx[k] === "string") out[k] = jsx[k];
  }
  return Object.keys(out).length ? { jsx: out } : null;
}
function extractEsbuild(es) {
  if (!es || typeof es !== "object") return null;
  const out = {};
  for (const k of JSX_ESBUILD_KEYS) if (typeof es[k] === "string") out[k] = es[k];
  return Object.keys(out).length ? out : null;
}

// `ssr.noExternal`/`external` entries: strings and globs pass through, RegExps
// become `{ regex }`, `true` stays `true`.
function extractSsr(ssr, ssrEnvironment) {
  // Vite treats `ssr.*` as sugar for `environments.ssr.*`; the environment
  // spelling wins where both name the same option.
  const envResolve = ssrEnvironment && typeof ssrEnvironment === "object" ? ssrEnvironment.resolve : null;
  const merged = { ...(ssr && typeof ssr === "object" ? ssr : {}) };
  if (envResolve || merged.resolve) merged.resolve = { ...(merged.resolve ?? {}), ...(envResolve ?? {}) };
  ssr = Object.keys(merged).length ? merged : null;
  if (!ssr) return null;
  const list = (v) => {
    if (v === true) return true;
    const arr = Array.isArray(v) ? v : v == null ? [] : [v];
    const out = [];
    for (const e of arr) {
      if (typeof e === "string") out.push(e);
      else if (e instanceof RegExp) out.push({ regex: e.source });
    }
    return out;
  };
  const out = {};
  const ne = list(ssr.noExternal);
  const ex = list(ssr.external);
  if (ne === true || (Array.isArray(ne) && ne.length)) out.noExternal = ne;
  if (ex === true || (Array.isArray(ex) && ex.length)) out.external = ex;
  if (typeof ssr.target === "string") out.target = ssr.target;
  const res = extractResolve(ssr.resolve);
  if (res) out.resolve = res;
  return Object.keys(out).length ? out : null;
}
// Whether an environment is "runner-backed": its modules execute in a
// plugin-driven runtime (e.g. the Cloudflare plugin's workerd
// DevEnvironments), not in oj's own Node SSR runner. Vite-shaped rule:
// conditions never cross runtimes — the ssr environment's `resolve.conditions`
// then describe THAT runtime and Node-executing consumers (the Start loader,
// the unbundled SSR resolver) must take Vite's Node server defaults instead.
//
// The signal is Vite's own declaration mechanism, decided at config-extraction
// time (before the plugin host is up, so oj can build the loader's env): a
// plugin that drives its own dev runtime DECLARES it by returning
// `environments.<name>.dev.createEnvironment` from its `config` hook (the
// user's raw config may declare one directly). Vite's resolveConfig runs those
// hooks and merges each return into the user config (runConfigHook:
// `conf = mergeConfig(conf, res)`, so later hooks see earlier merges) and only
// LATER default-fills every environment's `dev.createEnvironment`
// (resolveDevEnvironmentOptions), so the merged PRE-default-fill config
// carries exactly the user's and the plugins' declarations. The RESOLVED
// config is useless here: after the fill, presence says nothing.
const declaresRunnerEnvironment = (cfg) =>
  !!cfg &&
  typeof cfg === "object" &&
  Object.values(cfg.environments ?? {}).some(
    (e) => e && e.dev && typeof e.dev.createEnvironment === "function",
  );

// Plugins whose `config` hooks this out-of-band re-run must not execute: the
// exact rule the Start loader applies when IT re-runs config hooks on fresh
// instances (start/vite-plugin-bridge.mjs `ojReimplemented`) — the framework
// plugins oj reimplements natively never ran hooks under oj, and their config
// hooks have side effects that fight oj's own lifecycle (the TanStack router
// plugin's config hook starts its route generator) — plus the dev-tooling
// plugins the plugin host refuses outright (plugin-host.mjs
// OJ_UNSUPPORTED_PLUGIN_NAMES: vite-plugin-checker spawns a tsc/eslint
// worker). Tradeoff, stated plainly: a skipped hook is never run, so an
// `environments` declaration it WOULD return is invisible to this detection
// (a return cannot be captured without running the hook). That is acceptable:
// the skipped set is oj-reimplemented framework plugins, whose runtimes oj
// itself provides, and dev-only tooling, which declares no environments. The
// Cloudflare plugins match neither rule, so their declaration is seen.
const ojReimplemented = (name = "") =>
  name.startsWith("vite:") || /^tanstack[-:]/.test(name) || name.startsWith("@tanstack/");
const SIDE_EFFECTFUL_CONFIG_PLUGIN_NAMES = new Set(["vite-plugin-checker"]);
const skipsConfigHook = (p) => {
  const name = (p && p.name) || "";
  return ojReimplemented(name) || SIDE_EFFECTFUL_CONFIG_PLUGIN_NAMES.has(name);
};

// Vite's BasicMinimalPluginContext surface, enough for a config hook to speak.
const configHookContext = {
  meta: { rollupVersion: "4.0.0", watchMode: false },
  debug() {},
  info() {},
  warn(m) {
    warn(typeof m === "string" ? m : (m && m.message) || String(m));
  },
  error(m) {
    throw m instanceof Error ? m : new Error((m && m.message) || String(m));
  },
};

async function detectSsrRunnerBacked(raw, configEnv, merge = mergeConfigLite) {
  if (!raw || typeof raw !== "object") return false;
  if (declaresRunnerEnvironment(raw)) return true;
  const env = {
    command: configEnv?.command ?? "serve",
    mode: configEnv?.mode ?? "development",
    isSsrBuild: configEnv?.isSsrBuild ?? false,
    isPreview: false,
  };
  // Vite's config.ts, mirrored on the raw config's own fresh plugin instances.
  // This port runs ONLY on the paths where resolveConfig never ran (the esbuild
  // and plain-import config loaders, or an app Vite too old to resolve): on the
  // resolveConfig path the detection sentinel rode Vite's own single hook run,
  // so no plugin instance ever has its config hooks run twice. Mechanics:
  // asyncFlatten the plugin list (a factory may return an array, possibly
  // promised), drop falsy entries, filter by `apply(config, env)` /
  // `apply === command` (filterPlugin), order by `enforce` (sortUserPlugins),
  // then by the config hook's own `order` (getSortedPluginsByHook), and call
  // each `handler.call(ctx, config, env)`, merging every return into the
  // running config so later hooks see earlier merges (runConfigHook).
  let list = Array.isArray(raw.plugins) ? raw.plugins : [];
  try {
    do {
      list = (await Promise.all(list)).flat(Infinity);
    } while (list.some((v) => v && typeof v.then === "function"));
  } catch {
    return false;
  }
  list = list.filter(Boolean);
  const applyConfig = { ...raw, mode: env.mode };
  list = list.filter((p) => {
    if (!p.apply) return true;
    try {
      return typeof p.apply === "function" ? !!p.apply(applyConfig, env) : p.apply === env.command;
    } catch {
      return false;
    }
  });
  const enforceRank = (p) => (p.enforce === "pre" ? -1 : p.enforce === "post" ? 1 : 0);
  const hookRank = (h) =>
    h && typeof h === "object" ? (h.order === "pre" ? -1 : h.order === "post" ? 1 : 0) : 0;
  // Vite's getSortedPluginsByHook walks the enforce-sorted plugin list and
  // splices `order: "pre"` hooks to the very FRONT of the result (and "post"
  // to the very back): the hook's own order is PRIMARY, enforce only orders
  // plugins within one order band (an enforce-pre plugin's plain hook runs
  // AFTER a plain plugin's order-pre hook).
  const hooksOf = (hookName) => {
    const withHook = list
      .map((p, i) => ({ p, i, hook: p[hookName] }))
      .filter((e) => e.hook && (typeof e.hook === "function" || typeof e.hook.handler === "function"));
    withHook.sort(
      (a, b) => hookRank(a.hook) - hookRank(b.hook) || enforceRank(a.p) - enforceRank(b.p) || a.i - b.i,
    );
    return withHook;
  };
  let conf = { ...raw, plugins: list };
  for (const { p, hook } of hooksOf("config")) {
    if (skipsConfigHook(p)) continue;
    const handler = typeof hook === "object" ? hook.handler : hook;
    let res;
    try {
      res = await handler.call(configHookContext, conf, env);
    } catch (e) {
      // A hook error (a throw, or the hook calling this.error) must not fail
      // extraction, but it IS a real failure of that plugin's evaluation:
      // Vite aborts here, oj deliberately degrades to no-declaration from
      // this plugin — loudly, naming it.
      warn(`config hook of plugin "${p.name ?? "?"}" failed during extraction: ${(e && e.message) || e}`);
      continue;
    }
    if (res && res !== conf) {
      conf = merge(conf, res);
      // Vite resolves the plugin list once; every hook sees the same array.
      conf.plugins = list;
    }
  }
  if (declaresRunnerEnvironment(conf)) return true;
  // Vite fills the implicit environments after the config hooks (resolveConfig:
  // `config.environments ??= {}`, then the ssr and client entries), and only
  // then runs every plugin's configEnvironment hook once per environment name,
  // merging each return into that environment (runConfigEnvironmentHook) —
  // still BEFORE the default dev.createEnvironment fill, so a factory declared
  // here is a real declaration. Same skip lists and per-hook guard as above.
  const environments = { ...(conf.environments ?? {}) };
  const isBuild = env.command === "build";
  if (!environments.ssr && (!isBuild || conf.ssr || conf.build?.ssr)) environments.ssr = {};
  if (!environments.client) environments.client = {};
  for (const { p, hook } of hooksOf("configEnvironment")) {
    if (skipsConfigHook(p)) continue;
    const handler = typeof hook === "object" ? hook.handler : hook;
    for (const name of Object.keys(environments)) {
      let res;
      try {
        res = await handler.call(configHookContext, name, environments[name], {
          ...env,
          isSsrTargetWebworker: conf.ssr?.target === "webworker" && name === "ssr",
        });
      } catch (e) {
        warn(
          `configEnvironment hook of plugin "${p.name ?? "?"}" failed during extraction: ${(e && e.message) || e}`,
        );
        continue;
      }
      if (res) environments[name] = merge(environments[name], res);
    }
  }
  return declaresRunnerEnvironment({ environments });
}

// Here: the fallback merge when the app's own vite.mergeConfig is unavailable
// (the esbuild and plain-import loaders).
//

function extractResolve(r) {
  if (!r || typeof r !== "object") return null;
  const out = {};
  const strArr = (v) => (Array.isArray(v) ? v.filter((x) => typeof x === "string") : null);
  for (const k of ["extensions", "mainFields", "conditions", "externalConditions"]) {
    const v = strArr(r[k]);
    if (v && v.length) out[k] = v;
  }
  if (typeof r.preserveSymlinks === "boolean") out.preserveSymlinks = r.preserveSymlinks;
  return Object.keys(out).length ? out : null;
}
function extractServerFlags(s, legacy, appType) {
  const out = {};
  if (appType === "spa" || appType === "mpa" || appType === "custom") out.appType = appType;
  if (s && typeof s === "object") {
    if (typeof s.strictPort === "boolean") out.strictPort = s.strictPort;
    // Vite admits `open: true | string`; oj opens the served url in both cases.
    if (s.open === true || typeof s.open === "string") out.open = true;
    else if (s.open === false) out.open = false;
    // server.hmr object options reach the served client (Vite's clientInjections).
    if (s.hmr && typeof s.hmr === "object") {
      const h = {};
      for (const k of ["path", "host", "protocol"]) if (typeof s.hmr[k] === "string") h[k] = s.hmr[k];
      for (const k of ["port", "clientPort", "timeout"]) if (typeof s.hmr[k] === "number") h[k] = s.hmr[k];
      if (typeof s.hmr.overlay === "boolean") h.overlay = s.hmr.overlay;
      if (Object.keys(h).length) out.hmr = h;
    }
    if (s.fs && typeof s.fs === "object" && typeof s.fs.strict === "boolean") out.fsStrict = s.fs.strict;
    // server.watch.ignored: string globs only (RegExp/functions cannot cross the bridge).
    if (s.watch && typeof s.watch === "object" && s.watch.ignored != null) {
      const raw = Array.isArray(s.watch.ignored) ? s.watch.ignored : [s.watch.ignored];
      const ignored = raw.filter((x) => typeof x === "string");
      if (ignored.length) out.watch = { ignored };
      if (ignored.length !== raw.length) {
        warn("server.watch.ignored RegExp or function entries are not applied (string globs are)");
      }
    }
  }
  if (legacy?.skipWebSocketTokenCheck === true) out.skipWebSocketTokenCheck = true;
  return Object.keys(out).length ? out : null;
}
// `preview.*` as resolved by Vite (inheriting `server.*` except the port).
function extractPreview(p) {
  if (!p || typeof p !== "object") return null;
  const out = {};
  if (typeof p.port === "number") out.port = p.port;
  if (typeof p.host === "string") out.host = p.host;
  else if (p.host === true) out.host = "true";
  if (typeof p.strictPort === "boolean") out.strictPort = p.strictPort;
  if (p.open === true || typeof p.open === "string") out.open = p.open;
  const cors = extractCors(p.cors);
  if (cors !== null && cors !== undefined) out.cors = cors;
  const hosts = extractAllowedHosts(p.allowedHosts);
  if (hosts !== null && hosts !== undefined) out.allowedHosts = hosts;
  const headers = stringMap(p.headers);
  if (headers) out.headers = headers;
  if (p.proxy && typeof p.proxy === "object" && Object.keys(p.proxy).length) out.proxy = {};
  return Object.keys(out).length ? out : null;
}
function extractEnvPrefix(p) {
  if (typeof p === "string") return [p];
  if (Array.isArray(p)) return p.filter((x) => typeof x === "string");
  return null;
}
function extractCors(cors) {
  if (typeof cors === "boolean") return cors;
  if (!cors || typeof cors !== "object") return null;
  const out = {};
  const strOrList = (v) =>
    typeof v === "string" ? v : Array.isArray(v) ? v.filter((x) => typeof x === "string") : undefined;
  if (cors.origin === true || cors.origin === false) out.origin = cors.origin;
  else if (strOrList(cors.origin) !== undefined) out.origin = strOrList(cors.origin);
  if (strOrList(cors.methods) !== undefined) out.methods = strOrList(cors.methods);
  if (strOrList(cors.allowedHeaders) !== undefined) out.allowedHeaders = strOrList(cors.allowedHeaders);
  if (typeof cors.credentials === "boolean") out.credentials = cors.credentials;
  if (typeof cors.maxAge === "number") out.maxAge = cors.maxAge;
  return out;
}
function extractAllowedHosts(v) {
  if (v === true) return true;
  if (Array.isArray(v)) return v.filter((x) => typeof x === "string");
  return null;
}

// The `css` block as JSON: preprocessorOptions (additionalData, loadPaths, Less
// and Stylus options), devSourcemap, modules. Function-valued options (an
// `additionalData` callback) cannot cross to Rust and are dropped with a warning.
function extractCss(css) {
  if (!css || typeof css !== "object") return null;
  const po = css.preprocessorOptions;
  if (po && typeof po === "object") {
    for (const [lang, opts] of Object.entries(po)) {
      if (opts && typeof opts.additionalData === "function") {
        warn(`css.preprocessorOptions.${lang}.additionalData is a function; only the string form is applied`);
      }
    }
  }
  if (css.modules && typeof css.modules === "object") {
    for (const k of ["localsConvention", "generateScopedName"]) {
      if (typeof css.modules[k] === "function") warn(`css.modules.${k} is a function; only the string form is applied`);
    }
    if (typeof css.modules.getJSON === "function") warn("css.modules.getJSON is not applied");
  }
  try {
    const out = JSON.parse(JSON.stringify(css));
    // RegExps (globalModulePaths) and functions do not survive JSON; mark them
    // the way rollup options are marked so Rust can read the sources.
    if (css.modules && typeof css.modules === "object") out.modules = markFunctions(css.modules);
    return out && Object.keys(out).length ? out : null;
  } catch {
    return null;
  }
}

// Warns about the options in the USER's config that oj does not apply. Call it
// with the raw config file's export, not the resolved config: Vite's resolveConfig
// fills every option with defaults (esbuild.jsxDev/charset/legalComments, worker,
// ssr.resolve, cors.origin, optimizeDeps.esbuildOptions, terserOptions) and none of
// those are configuration to warn about.
function warnUnsupported(c) {
  if (!c || typeof c !== "object") return;
  if (c.build?.terserOptions) warn("build.terserOptions is not applied (oj minifies with oxc)");
  if (c.esbuild?.jsx === "preserve" || c.oxc?.jsx === "preserve") {
    warn("jsx: \"preserve\" is not supported; JSX is compiled with the automatic runtime");
  }
  if (c.esbuild && typeof c.esbuild === "object") {
    const rest = Object.keys(c.esbuild).filter((k) => !JSX_ESBUILD_KEYS.includes(k));
    if (rest.length) warn(`esbuild options ${rest.join(", ")} are not applied (jsx* are)`);
  }
  if (c.optimizeDeps?.esbuildOptions || c.optimizeDeps?.rollupOptions) {
    warn("optimizeDeps.esbuildOptions/rollupOptions are not applied; include/exclude/entries are");
  }
  if (c.worker) warn("worker config is not applied");
  if (typeof c.build?.assetsInlineLimit === "function") {
    warn("build.assetsInlineLimit is a function and cannot be applied; the 4096 byte default is used");
  }
  if (c.ssr?.resolve && typeof c.ssr.resolve === "object") {
    // ssr.resolve.conditions/externalConditions ARE applied (the preferred
    // source for the Node SSR consumers); the remaining subkeys are inert.
    const inert = Object.keys(c.ssr.resolve).filter((k) => k !== "conditions" && k !== "externalConditions");
    if (inert.length) warn(`ssr.resolve.${inert.join("/")} is not applied (conditions/externalConditions are)`);
  }
  if (c.server?.cors && typeof c.server.cors === "object" && c.server.cors.origin instanceof RegExp) {
    warn("server.cors.origin RegExp is not applied; the localhost default is used");
  }
}

export { detectSsrRunnerBacked, extractAlias, extractOptimizeDeps, extractProxy, extractResolve, extractSsr, mergeConfigLite, warnUnsupported };

/// Evaluates the app's vite.config and returns the extracted values. Called by
/// oj through a short-lived in-process JS engine (one isolate per extraction),
/// so a config hook that starts a watcher or an interval dies with the engine,
/// as it used to die with the one-shot subprocess. The RETURN value is the
/// result channel: `__ok: false` marks a config that failed to evaluate (a
/// broken config is not an empty config), `__deps`/`__depsTruncated` feed the
/// extraction cache, and `__stderr` carries everything the run would have
/// printed to stderr.
export async function extract(input) {
  configPath = String(input.vite);
  appRoot = String(input.root);
  command = input.command || "serve";
  mode = input.mode || "development";
  modeExplicit = input.modeKind !== "default";
  ojCacheDir = typeof input.cacheDir === "string" && input.cacheDir ? input.cacheDir : null;
  const chunks = [];
  installOutputCapture(chunks);
  isolateProcessForExtraction();
  process.env.VITE_CONFIG_NATIVE_IGNORE_WARNING ??= "true";
  defaultNodeEnv = command === "build" ? "production" : "development";
  nodeEnvWasSet = !!process.env.NODE_ENV;
  if (!nodeEnvWasSet) process.env.NODE_ENV = defaultNodeEnv;
  appRequire = createRequire(pathToFileURL(appRoot + "/package.json").href);
  directDeps = [];
  try {
    const pkg = JSON.parse(readFileSync(appRoot + "/package.json", "utf8"));
    directDeps = Object.keys({ ...pkg.dependencies, ...pkg.devDependencies });
  } catch {}
  observedConfigReads.clear();
  observedReadsTruncated = false;
try {
  installConfigReadRecorder();
  const { config, raw, deps, runnerBacked, merge } = (await loadConfig()) ?? {};
  const c = config ?? {};
  warnUnsupported(raw ?? c);
  // Extraction is the single runner-backed detection authority (the plugin
  // host and every Rust consumer read what it publishes). On the resolveConfig
  // path the sentinel decided inside Vite's own hook run; otherwise the port
  // runs the RAW config's fresh plugin instances (no resolved-config hooks ran
  // in this process then). Vite's mode rule (config.ts): an explicit inline
  // mode wins, else the config file's own. isSsrBuild: Vite computes it from
  // the INLINE config BEFORE the file loads (`config = inlineConfig`;
  // `isSsrBuild: command === "build" && !!config.build?.ssr`) and never
  // recomputes it, so a build.ssr that only the config FILE sets is invisible
  // to config hooks under Vite too; oj has no inline build.ssr at extraction
  // time, so it is false for serve AND build.
  const ssrRunnerBacked =
    typeof runnerBacked === "boolean"
      ? runnerBacked
      : await detectSsrRunnerBacked(
          raw,
          {
            command,
            mode: modeExplicit ? mode : typeof raw?.mode === "string" ? raw.mode : mode,
            isSsrBuild: false,
          },
          merge ?? mergeConfigLite,
        );
  let ssr = extractSsr(c.ssr, c.environments?.ssr);
  let resolveOut = extractResolve(c.resolve);
  if (raw == null) {
    // No raw config means detection could not run: the resolved ssr.resolve
    // conditions may describe a plugin's foreign runtime (workerd) that Node
    // consumers must never adopt undetected. Correctness over completeness:
    // withhold the sugar (loadConfig already warned) and let defaults stand.
    if (ssr) {
      delete ssr.resolve;
      if (Object.keys(ssr).length === 0) ssr = null;
    }
    // Same rule for the resolved TOP-LEVEL conditions: that list is Vite's
    // client-environment fill (browser-bearing, possibly plugin-extended) and
    // the Node consumers would adopt it with no detection to gate it — no
    // foreign conditions may reach them undetected on this path either.
    if (resolveOut) {
      delete resolveOut.conditions;
      delete resolveOut.externalConditions;
      if (Object.keys(resolveOut).length === 0) resolveOut = null;
    }
  }
  recordEnvFilesForMode(
    typeof c.mode === "string" ? c.mode : mode,
    typeof c.envDir === "string" ? c.envDir : null,
  );
  return {
      __ok: true,
      __stderr: chunks.join(""),
      // Config imports plus the config-shaped files (.json/.jsonc/.toml,
      // .env*) the evaluation read (or probed): both invalidate the
      // extraction cache when they change.
      __deps: [...new Set([...(deps ?? []), ...observedConfigReads])],
      // The recorder overflowed: __deps is incomplete, so the caller must
      // not cache this extraction under it (absent when the stamp is whole).
      ...(observedReadsTruncated ? { __depsTruncated: true } : {}),
      base: typeof c.base === "string" ? c.base : null,
      publicDir: typeof c.publicDir === "string" ? c.publicDir : c.publicDir === false ? false : null,
      port: typeof c.server?.port === "number" ? c.server.port : null,
      host: typeof c.server?.host === "string" ? c.server.host : null,
      hmr: c.server?.hmr === false ? false : null,
      fsAllow: Array.isArray(c.server?.fs?.allow)
        ? c.server.fs.allow.filter((x) => typeof x === "string")
        : null,
      fsStrict: typeof c.server?.fs?.strict === "boolean" ? c.server.fs.strict : null,
      define: c.define && typeof c.define === "object" ? c.define : null,
      alias: extractAlias(c.resolve?.alias),
      headers: stringMap(c.server?.headers),
      proxy: extractProxy(c.server?.proxy),
      rollupOptions: markFunctions(c.build?.rolldownOptions ?? c.build?.rollupOptions ?? null),
      assetsInlineLimit:
        typeof c.build?.assetsInlineLimit === "number" ? c.build.assetsInlineLimit : null,
      dedupe: Array.isArray(c.resolve?.dedupe)
        ? c.resolve.dedupe.filter((x) => typeof x === "string")
        : null,
      optimizeDeps: extractOptimizeDeps(c.optimizeDeps),
      build: extractBuild(c.build),
      oxc: extractOxc(c.oxc),
      esbuild: extractEsbuild(c.esbuild),
      ssr: ssrRunnerBacked ? { ...(ssr ?? {}), runnerBacked: true } : ssr,
      mode: typeof c.mode === "string" ? c.mode : null,
      resolve: resolveOut,
      // The RAW config file's own top-level `resolve` block. The resolved
      // config's `resolve.conditions` is Vite's CLIENT environment list
      // (browser-bearing defaults) — never a server-side conditions source —
      // while this one is user-authored and runtime-neutral: the Node SSR
      // consumers add it to their Node defaults when ssr is runner-backed.
      rawResolve: extractResolve(raw?.resolve),
      serverFlags: extractServerFlags(c.server, c.legacy, c.appType),
      css: extractCss(c.css),
      envPrefix: extractEnvPrefix(c.envPrefix),
      envDir: typeof c.envDir === "string" ? c.envDir : null,
      cors: extractCors(c.server?.cors),
      allowedHosts: extractAllowedHosts(c.server?.allowedHosts),
      preview: extractPreview(c.preview),
      appType: typeof c.appType === "string" ? c.appType : null,
      html: typeof c.html?.cspNonce === "string" ? { cspNonce: c.html.cspNonce } : null,
  };
} catch (e) {
  emitStderr(`oj: could not extract vite.config values: ${(e && e.stack) || e}\n`);
  // A config that failed to evaluate is not a config with no values: __ok
  // stays false and the caller refuses to serve defaults under it.
  return { __ok: false, __stderr: chunks.join("") };
}
}
