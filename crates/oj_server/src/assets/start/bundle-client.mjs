// SPDX-License-Identifier: MIT

import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, isAbsolute, join, resolve, relative } from "node:path";
import { fileURLToPath } from "node:url";
import {
  freeRolldownOutputs,
  importPkg,
  importOjRolldown,
  importOjRolldownExperimental,
  importOjRolldownParseAst,
  moduleParser,
  viteEnvDefine,
  environmentDefines,
  jsxTransformOptions,
  phase,
} from "./resolve-pkg.mjs";
import {
  appAliasPlugin,
  assetsPlugin,
  buildErrorPayload,
  clientExternal,
  clientChunkPolicy,
  devSafeChunkPolicy,
  makeVitePlugins,
  nodeBuiltinShims,
  devWatchOptions,
  unresolvedImportIn,
  unresolvedImportInPatch,
  viteOnLog,
  workspaceRoot,
} from "./rolldown-assets.mjs";
import { loadPluginContainer } from "./vite-plugin-bridge.mjs";
import { transformGlob, transformImportMetaUrl, depImportMetaUrlPlugin, bundlerResolve } from "./glob-transform.mjs";
import { loadViteGlobs } from "./vite-globs.mjs";
import { rewriteClientServerFns } from "./server-fns.mjs";
import { pathToFileURL } from "node:url";

// Runs on oj's embedded engine (`ScriptEngine`): the values that used to be
// spawn env arrive as `env`; `node <script>` still works for tests.
export async function run(env = null) {
  if (env) for (const [k, v] of Object.entries(env)) process.env[k] = v;
  // The node spawn ran with cwd = the app root; cwd-relative plugin filters
  // (svgr's createFilter globs) depend on it.
  if (env && env.OJ_APP_ROOT) {
    try {
      process.chdir(env.OJ_APP_ROOT);
    } catch {}
  }
  return main();
}

// One plugin container per dev server, as Vite has: `oj dev` keeps this
// script's engine alive across rebundles (`start-script --serve`), so every
// rebundle reuses the same plugin instances and buildStart runs once. A
// plugin's state survives rebuilds; a fresh container per rebundle made a
// plugin that writes a generated file from buildStart rewrite it on every
// bundle, and the watcher fed that write back as the next rebundle.
let containerLoad = null;
// Bundles this engine ran: the `?inline` sheets' import key.
let bundles = 0;

function pluginContainer(app, mode) {
  containerLoad ??= loadPluginContainer(app, { command: "serve", mode, environment: "client" }).catch((e) => {
    containerLoad = null;
    throw e;
  });
  return containerLoad;
}

// The server closing: Vite's container.close() runs buildEnd once, at the
// end of the session, not after each request.
export async function close() {
  await live?.engine.close().catch(() => {});
  const container = await containerLoad?.catch(() => null);
  if (container?.buildEnd) await container.buildEnd();
}

// Hot updates, as Vite's bundled dev serves them: one rolldown DevEngine for
// the session (server/bundledDev.ts) instead of a build per edit. Its own
// watcher turns an edit into a patch per page (the changed modules alone)
// within milliseconds; the dev server pushes each patch to its page, whose
// runtime (hmr-runtime.js) re-runs the modules that accept it. A rebundle
// request still answers with the full, current output, for reloads and the
// bundle cache. `live` is that engine and the state its plugins write into.
let live = null;
// Pages that announced themselves (oj:client), registered with every engine.
const clients = new Set();
// What the engine's watcher made of each changed file since the last
// rebundle: "patch", "noop" or "reload".
const seen = new Map();
// The engine's last failed update (a syntax error), for the rebundle that
// reports it.
let hotError = null;
// Prefixes an unsolicited line to the dev server (start_dev.rs reads it).
const EVENT = "\u001eoj-start-event:";

// Requests from the dev server outside a rebundle, one per page event.
export async function op(msg) {
  if (msg.op === "client") {
    clients.add(msg.id);
    await live?.engine.registerClient(msg.id);
  } else if (msg.op === "gone") {
    clients.delete(msg.id);
    await live?.engine.removeClient(msg.id);
  } else if (msg.op === "delivered") {
    await live?.engine.notifyPayloadDelivered(msg.filename);
  }
}

// The answer to a rebundle request, written by start-script --serve through
// this same stream so it never splits an event line.
export function reply(line) {
  return new Promise((resolve) => process.stdout.write(line, () => resolve()));
}

function emit(event) {
  process.stdout.write(EVENT + JSON.stringify(event) + "\n");
}

function onHmrUpdates(result) {
  // A failed update reaches the page through the rebundle that follows.
  if (result instanceof Error) {
    hotError = result;
    return;
  }
  hotError = null;
  const { updates, changedFiles } = result;
  const kinds = updates.map((u) => u.update.type);
  const kind = kinds.includes("FullReload") ? "reload" : kinds.some((k) => k === "Patch") ? "patch" : "noop";
  for (const file of changedFiles) seen.set(file, kinds.length ? kind : "unseen");
  for (const { clientId, update } of updates) {
    if (update.type !== "Patch") continue;
    // An import that never resolved fails the rebundle (the overlay), never
    // the page that would run the patch.
    const unresolved = unresolvedImportInPatch(
      update.code,
      resolve(process.cwd(), update.changedIds[0] ?? ""),
      live?.config,
      live?.parseSync,
    );
    if (unresolved) {
      hotError = unresolved;
      continue;
    }
    emit({
      kind: "patch",
      client: clientId,
      filename: update.filename,
      seq: update.seq,
      changedIds: update.changedIds,
      code: update.code,
    });
  }
}

const until = async (ready, ms) => {
  const end = Date.now() + ms;
  while (!ready()) {
    if (Date.now() > end) return false;
    await new Promise((r) => setTimeout(r, 5));
  }
  return true;
};

// The session's engine, built once (devMode, not lazy: the whole client in
// one output, as the bundles before it). It shares the build's options; the
// runtime it inlines is hmr-runtime.js.
async function startEngine(experimental, options, state, epoch, config) {
  const here = dirname(fileURLToPath(import.meta.url));
  const { output, ...input } = options;
  input.experimental = {
    ...input.experimental,
    devMode: { lazy: false, implement: readFileSync(join(here, "hmr-runtime.js"), "utf8") },
  };
  // Vite turns this off for its bundled dev too (vitejs/vite#21843).
  input.optimization = { ...input.optimization, inlineConst: false };
  const engine = {
    engine: null,
    state,
    epoch,
    closure: new Set(),
    output: null,
    outputs: 0,
    error: null,
    config,
    parseSync: experimental.parseSync,
  };
  engine.engine = await experimental.dev(input, output, {
    onHmrUpdates,
    onOutput: (result) => {
      if (result instanceof Error) {
        engine.error = result;
      } else {
        engine.error = null;
        engine.output = result.output;
      }
      engine.outputs++;
    },
    watch: { skipWrite: true, ...devWatchOptions(config) },
  });
  engine.engine.run().catch(() => {});
  for (const id of clients) await engine.engine.registerClient(id);
  return engine;
}

// The engine's current output: what a rebundle writes for reloads and the
// bundle cache. Waits out a build the engine still owes.
async function latestOutput(live) {
  const before = live.outputs;
  await live.engine.ensureCurrentBuildFinish();
  const state = await live.engine.getBundleState();
  // A failed update leaves the engine waiting on the next edit; Vite forces
  // the full build that recovers it when a page needs the output.
  if (state.lastBuildErrored && state.lastErrorStage === "Hmr") live.engine.triggerFullBuild();
  if (state.hasStaleOutput || state.lastBuildErrored || !live.output) {
    await live.engine.ensureLatestBuildOutput();
    // The output callback lands right after the build it reports; none at
    // all means the engine had nothing newer (a failure no edit fixed yet).
    await until(() => live.outputs !== before, 2000);
  }
  if (live.error) throw live.error;
  if (!live.output) throw new Error("the client bundle produced no output");
  const unresolved = unresolvedImportIn(live.output, live.config);
  if (unresolved) throw unresolved;
  return live.output;
}

// Whether a rebundle's edits all went out as hot updates. The engine watches
// the same files the dev server does, usually sooner; an input it has no
// module for (a file the bundle only declared, a create under a glob's scan
// base) rebuilds in full.
async function settle(live, changed) {
  // The engine's own graph where it exposes one (rolldown 1.2.5+), else the
  // last output's inputs.
  const graph = live.engine.moduleGraph;
  const known = graph ? (p) => graph.getModuleInfo(p) != null : (p) => live.closure.has(p);
  const modules = changed.filter(known);
  const watched = await until(() => hotError || modules.every((p) => seen.has(p)), 1000);
  if (hotError) {
    const error = hotError;
    hotError = null;
    for (const p of modules) seen.delete(p);
    throw error;
  }
  const kinds = modules.map((p) => seen.get(p));
  for (const p of modules) seen.delete(p);
  const unknown = changed.length > modules.length;
  if (unknown || !watched) live.engine.triggerFullBuild();
  return (
    clients.size > 0 && modules.length > 0 && watched && !unknown && kinds.every((k) => k === "patch" || k === "noop")
  );
}

// The stylesheets a hot update imported for the first time: the server
// render links them from now on (manifest-dev.ts reads css-urls.json per
// request), and the dev server links them in the open pages. Under Vite a
// CSS import is a module that injects its own sheet; here it is a link.
function linkNewStyles(dir, cssUrls) {
  const file = join(dir, "css-urls.json");
  let linked = [];
  try {
    linked = JSON.parse(readFileSync(file, "utf8"));
  } catch {}
  const added = cssUrls.filter((href) => !linked.includes(href));
  if (added.length) {
    writeFileSync(file + ".tmp", JSON.stringify(cssUrls));
    renameSync(file + ".tmp", file);
  }
  return added;
}

async function main() {
  const HERE = dirname(fileURLToPath(import.meta.url));
  const APP = process.env.OJ_APP_ROOT ?? process.cwd();
  const _ojTTY = process.stderr.isTTY && !process.env.NO_COLOR;
  const OJ = _ojTTY ? "\x1b[48;2;255;255;255m\x1b[1;38;2;42;51;212m oj \x1b[0m" : "oj";
  // Before the rolldown import and the build: a bundle that hangs with no
  // output is indistinguishable from one that never started.
  process.stderr.write(`${OJ}${_ojTTY ? "" : ":"} client bundling\n`);
  const { build } = await importOjRolldown(APP, ["vite", "@tanstack/react-start"]);
  await loadViteGlobs(APP);
  const parseAst = await importOjRolldownParseAst(APP, ["vite", "@tanstack/react-start"]);
  phase("client bundle: rolldown imported");
  const WORKSPACE = workspaceRoot(APP);
  const SERVER_FN_BASE = process.env.TSS_SERVER_FN_BASE ?? "/_serverFn/";
  // Vite's `--mode` (OJ_MODE from oj; `development` for `oj dev` without one).
  const MODE = process.env.OJ_MODE || "development";
  // Set by `oj dev` per Vite's NODE_ENV rule (shell wins, else .env, else
  // development): `NODE_ENV=production oj dev` compiles a PROD client like Vite.
  const NODE_ENV = process.env.NODE_ENV || "development";
  // The config's `define` map (OJ_DEFINE from oj). Vite's define plugin applies
  // it to the client environment exactly as to SSR, so a define a component
  // references must not render on the server and then throw on hydration.
  // Config defines win over Vite's env defines, as in the SSR loader.
  const USER_DEFINE = (() => {
    try {
      return JSON.parse(process.env.OJ_DEFINE || "{}") || {};
    } catch {
      return {};
    }
  })();
  // The client environment's export conditions (OJ_CLIENT_CONDITIONS from oj):
  // Vite derives them from `resolve.conditions`, so a user list reaches this
  // bundle exactly as it reaches the dev server's resolver.
  const CLIENT_CONDITIONS = (() => {
    try {
      const v = JSON.parse(process.env.OJ_CLIENT_CONDITIONS || "null");
      if (Array.isArray(v) && v.every((c) => typeof c === "string") && v.length) return v;
    } catch {}
    return ["browser", "module", "import", "development"];
  })();

  // The app's package.json "imports" wins over the src/router convention, the
  // way it does in the SSR loader: the framework makes this path configurable
  // (`router.entry`), and an app that moved it has to be able to say so.
  function declaredRouterEntry() {
    try {
      const target = JSON.parse(readFileSync(resolve(APP, "package.json"), "utf8")).imports?.["#tanstack-router-entry"];
      if (typeof target !== "string") return null;
      const p = resolve(APP, target);
      return existsSync(p) ? p : null;
    } catch {
      return null;
    }
  }

  function routerEntry() {
    const declared = declaredRouterEntry();
    if (declared) return declared;
    for (const ext of [".tsx", ".ts", ".jsx", ".js"]) {
      const p = resolve(APP, "src/router" + ext);
      if (existsSync(p)) return p;
    }
    return resolve(APP, "src/router.tsx");
  }

  // `#tanstack-start-entry` resolves to the app's `src/start.*` exactly as
  // upstream's resolveEntry does (`startFilePath ?? defaultEntryPaths.start`),
  // falling back to the stub that mirrors the framework's default entry
  // (`export const startInstance = undefined`). The client needs the real file
  // too: `functionMiddleware` wraps server-fn calls in the browser.
  function startEntry() {
    try {
      const target = JSON.parse(readFileSync(resolve(APP, "package.json"), "utf8")).imports?.["#tanstack-start-entry"];
      if (typeof target === "string") {
        const p = resolve(APP, target);
        if (existsSync(p)) return p;
      }
    } catch {}
    for (const ext of [".ts", ".js", ".mts", ".mjs", ".tsx", ".jsx"]) {
      const p = resolve(APP, "src/start" + ext);
      if (existsSync(p)) return p;
    }
    return join(HERE, "start-entry.ts");
  }

  const container = await pluginContainer(APP, MODE);
  phase("client bundle: plugin container loaded");

  const cssUrls = [];
  // Every import.meta.glob this bundle expanded, as match specs; the dev
  // rebundle gate rebundles for a CREATED file one matches (the glob's
  // expansion grows), and for no other create, mirroring Vite's add handling.
  const globScopes = new Set();

  const visitedIds = new Set();
  const closureRecorder = {
    name: "oj-closure-recorder",
    transform(code, id) {
      visitedIds.add(id);
      return null;
    },
    // The complete input set, from the bundler itself: getModuleIds() covers
    // every module the build touched, including the kinds the transform hook
    // and the output walk miss (css imports among them). closure.json must
    // not under-report: the dev server skips rebundles for paths outside it.
    buildEnd() {
      try {
        for (const id of this.getModuleIds()) visitedIds.add(id);
      } catch {}
    },
  };

  const serverFnClient = {
    name: "server-fn-client",
    transform: {
      filter: { id: { include: /\.(tsx?|jsx?|mjs)$/, exclude: [/\/node_modules\//, /^\0/] } },
      async handler(code, id) {
        if (id.includes("/node_modules/") || id.startsWith("\0") || !/\.(tsx?|jsx?|mjs)$/.test(id)) return null;
        let out = code;
        if (container) {
          const t = await container.transformUserCode(out, id);
          if (t != null) out = t;
        }
        out = await transformImportMetaUrl(out, id, bundlerResolve(this));
        out = transformGlob(out, id, undefined, globScopes);
        const rpc = rewriteClientServerFns(out, relative(APP, id), moduleParser(parseAst, id));
        if (rpc != null) return rpc;
        return out === code ? null : out;
      },
    },
  };

  // React Fast Refresh in the bundle, as @vitejs/plugin-react sets it up for
  // Vite's bundled dev: oxc's refresh transform on app sources (rolldown's
  // native vite:oxc), then the wrapper that registers each module's exports
  // and accepts the update when they are all components. The runtime is
  // oj's copy of plugin-react's, and its preamble runs first in the entry,
  // before react-dom loads. Only for an app with the react plugin, as in Vite.
  function withFastRefresh(experimental, options) {
    const react = [container?.config?.plugins ?? []].flat(Infinity).some((p) => p?.name === "vite:react-refresh");
    if (!react || typeof experimental.viteReactRefreshWrapperPlugin !== "function") return options;
    const runtime = join(HERE, "react-refresh-runtime.js");
    const entry = join(HERE, "client-entry.tsx");
    const preamble = "\0oj-react-refresh-preamble";
    const include = [/\.[tj]sx?(?:$|\?)/];
    const exclude = [/\/node_modules\//];
    const jsx = { ...jsxTransformOptions(true), refresh: true };
    return {
      ...options,
      plugins: [
        {
          name: "oj-react-refresh",
          // Ahead of plugin-react's own pre-ordered resolveId, which would
          // claim the id for a load oj does not run.
          resolveId: {
            order: "pre",
            handler(id) {
              if (id === "/@react-refresh") return runtime;
              if (id === preamble) return id;
            },
          },
          load(id) {
            if (id === preamble) {
              return [
                'import { injectIntoGlobalHook } from "/@react-refresh";',
                "injectIntoGlobalHook(window);",
                "window.$RefreshReg$ = () => {};",
                "window.$RefreshSig$ = () => (type) => type;",
              ].join("\n");
            }
          },
          transform(code, id) {
            if (id === entry) return `import ${JSON.stringify(preamble)}; ${code}`;
          },
        },
        ...options.plugins,
        experimental.viteTransformPlugin({
          root: APP,
          include: /\.(m?ts|[jt]sx)$/,
          exclude: [/\.js$/, /\/node_modules\//],
          jsxRefreshInclude: include,
          jsxRefreshExclude: exclude,
          isServerConsumer: false,
          transformOptions: { jsx, sourcemap: true },
        }),
        experimental.viteReactRefreshWrapperPlugin({
          cwd: APP,
          include,
          exclude,
          jsxImportSource: jsx.importSource ?? "react",
          reactRefreshHost: "",
        }),
      ],
    };
  }

  const chunkPolicy = devSafeChunkPolicy(clientChunkPolicy(container?.config));
  phase("client bundle: build starting");
  // The last failure, as Vite's bundled dev keeps lastBuildError: the dev
  // server replays it to every page as the error overlay until a bundle
  // succeeds.
  const errorFile = join(HERE, "client-error.json");
  // Whether this rebundle's edits all reached the pages as hot updates (the
  // dev server then skips the reload).
  const hmrFile = join(HERE, "client-hmr.json");
  rmSync(hmrFile, { force: true });
  const epoch = process.env.OJ_START_STYLE_EPOCH ?? "0";
  ++bundles;
  // Vite's bundled dev runs only when HMR is on: never for a production
  // client (`NODE_ENV=production oj dev`) or `server.hmr: false`. Nor under
  // the editor's HMR gate (OJ_START_HOT=0): its pages reload on every edit,
  // so they never take a patch, and a plain build serves them as before.
  const experimental =
    NODE_ENV !== "production" && container?.config?.server?.hmr !== false && process.env.OJ_START_HOT !== "0"
      ? await importOjRolldownExperimental(APP, ["vite", "@tanstack/react-start"])
      : null;
  const hot = typeof experimental?.dev === "function";
  // A style input moved: CSS modules and preprocessor sheets load under the
  // epoch, which the running engine's cached modules predate, and a partial
  // is no module of its graph at all. A fresh engine compiles them anew (a
  // reload follows, as before). An engine that compiled no sheet (plain CSS
  // is linked, never compiled) has nothing the epoch could change: it keeps
  // running, so a new plain stylesheet goes out with the update importing it.
  if (live && (!hot || (live.epoch !== epoch && live.state.compiledSheets))) {
    await live.engine.close().catch(() => {});
    live = null;
  }
  if (live) live.epoch = epoch;
  const state = live?.state ?? { cssUrls, globScopes, visitedIds, compiledSheets: false };
  // A sheet compiled from now on compiles under the current epoch.
  state.epoch = epoch;
  let result;
  let hmrHandled = false;
  try {
    if (live) {
      if (process.env.OJ_START_REFRESH !== "1") {
        hmrHandled = await settle(live, JSON.parse(process.env.OJ_START_CHANGED || "[]"));
      }
      // The pages run the edit already: the full output waits for the dev
      // server's refresh once edits pause, off the next edit's path.
      if (hmrHandled) {
        rmSync(errorFile, { force: true });
        writeFileSync(hmrFile, JSON.stringify({ handled: true, css: linkNewStyles(HERE, state.cssUrls) }));
        return;
      }
      result = { output: await latestOutput(live) };
    } else {
      const options = {
        input: join(HERE, "client-entry.tsx"),
        platform: "browser",
        ...chunkPolicy.input,
        ...clientExternal(container?.config),
        onLog: viteOnLog,
        transform: {
          jsx: jsxTransformOptions(NODE_ENV !== "production"),
          define: {
            "process.env": JSON.stringify({ NODE_ENV, TSS_SERVER_FN_BASE: SERVER_FN_BASE }),
            // Explicit, like Vite's define plugin. It also guards a regression if
            // minify is ever enabled: rolldown's minifier folds the key to
            // "production" and React picks its prod jsx runtime (no jsxDEV).
            "process.env.NODE_ENV": JSON.stringify(NODE_ENV),
            global: "globalThis",
            ...viteEnvDefine({ ssr: false, mode: MODE }),
            ...USER_DEFINE,
            ...environmentDefines("client"),
            ...(container?.defines?.() ?? {}),
          },
        },
        resolve: {
          conditionNames: CLIENT_CONDITIONS,
          alias: {
            "#tanstack-router-entry": routerEntry(),
            "#tanstack-start-entry": startEntry(),
            "#tanstack-start-plugin-adapters": join(HERE, "plugin-adapters.ts"),
            "tanstack-start-manifest:v": join(HERE, "manifest.ts"),
            "tanstack-start-injected-head-scripts:v": join(HERE, "injected-head-scripts.ts"),
            "@tanstack/start-fn-stubs": join(HERE, "fn-stubs.mjs"),
          },
        },
        plugins: [
          closureRecorder,
          appAliasPlugin(container?.config),
          makeVitePlugins({ container, appRoot: APP, mode: "dev", buildEnd: false }),
          assetsPlugin({
            mode: "dev",
            cssUrls,
            root: APP,
            // Read per load: an engine outlives the rebundle that created it.
            styleKey: {
              // Read only for a sheet oj compiles (a CSS module or a
              // preprocessor file).
              get css() {
                state.compiledSheets = true;
                return { epoch: state.epoch };
              },
              get inline() {
                return { bundle: String(bundles) };
              },
            },
          }),
          serverFnClient,
          depImportMetaUrlPlugin(),
          nodeBuiltinShims({ production: false }),
        ],
        output: {
          format: "esm",
          banner:
            `globalThis.process=globalThis.process||{env:{NODE_ENV:${JSON.stringify(NODE_ENV)},TSS_SERVER_FN_BASE:${JSON.stringify(SERVER_FN_BASE)}}};` +
            "globalThis.global=globalThis.global||globalThis;",
          ...chunkPolicy.output,
        },
      };
      if (hot) {
        live = await startEngine(experimental, withFastRefresh(experimental, options), state, epoch, container?.config);
        result = { output: await latestOutput(live) };
      } else {
        result = await build({ ...options, write: false });
      }
    }
  } catch (error) {
    writeFileSync(errorFile, JSON.stringify(buildErrorPayload(error)));
    throw error;
  }
  rmSync(errorFile, { force: true });
  writeFileSync(hmrFile, JSON.stringify({ handled: hmrHandled }));
  const { cssUrls: liveCssUrls, globScopes: liveGlobScopes, visitedIds: liveVisitedIds } = state;

  const entryChunk = result.output.find((o) => o.type === "chunk" && o.isEntry) ?? result.output[0];
  const chunksDir = join(HERE, "client-chunks");
  rmSync(chunksDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  mkdirSync(chunksDir, { recursive: true });
  const chunkIndex = [];
  let moduleCount = 0;
  for (const o of result.output) {
    const dest = join(chunksDir, o.fileName);
    mkdirSync(dirname(dest), { recursive: true });
    writeFileSync(dest, o.type === "chunk" ? o.code : o.source);
    chunkIndex.push({ name: o.fileName, size: statSync(dest).size });
    if (o.type === "chunk") moduleCount += Object.keys(o.modules ?? {}).length;
  }
  writeFileSync(join(HERE, "client-chunks.json"), JSON.stringify({ entry: entryChunk.fileName, files: chunkIndex }));
  // Module count for the editor's update-progress narration ("(N modules)").
  writeFileSync(join(HERE, "client-entry.modules"), String(moduleCount));

  phase(`client bundle: build done (${result.output.length} files)`);
  const closure = new Set([
    routerEntry(),
    ...liveVisitedIds,
    ...(container?.configDependencies ?? []),
    ...(container?.watchFiles ?? []),
  ]);
  for (const o of result.output) {
    if (o.type !== "chunk") continue;
    for (const id of Object.keys(o.modules ?? {})) closure.add(id);
  }
  const closureFiles = [...closure]
    // An oj module id wraps a file (\0oj-vite-virtual:/abs/x.ts?q), and an
    // asset id carries a query (/abs/x.css?inline); the FILE is the input, or
    // an edit to it never rebundles.
    .map((id) => {
      const s = String(id);
      return s.startsWith("\0oj-") ? s.slice(s.indexOf(":") + 1) : s;
    })
    .map((id) => id.split("?")[0])
    .filter((p) => !p.startsWith("\0"))
    .map((p) => (isAbsolute(p) ? p : resolve(APP, p)))
    .filter((p) => !p.startsWith(HERE) && !/(^|\/)routeTree\.gen\.[jt]sx?$/.test(p))
    .filter((p) => {
      try {
        return statSync(p).isFile();
      } catch {
        return false;
      }
    })
    .sort();
  writeFileSync(join(HERE, "closure.json"), JSON.stringify(closureFiles));
  writeFileSync(join(HERE, "glob-scopes.json"), JSON.stringify([...liveGlobScopes].sort().map((s) => JSON.parse(s))));
  if (live) live.closure = new Set(closureFiles);

  phase("client bundle: closure written");
  const devManifest = {
    routes: {
      __root__: {
        preloads: ["/@oj-start/client-entry.js"],
        css: liveCssUrls,
        scripts: [{ attrs: { type: "module", async: true, src: "/@oj-start/client-entry.js" } }],
      },
    },
  };
  // Only a changed manifest is written: the client bundle imports this file,
  // so the engine's watcher takes every write as an edit. Under the engine it
  // accepts its own update: the pages read it once, at hydration, and a
  // sheet it adds reached them already (linkNewStyles).
  const manifest =
    `export const tsrStartManifest = () => (${JSON.stringify(devManifest)});\n` +
    (live ? "if (import.meta.hot) import.meta.hot.accept();\n" : "");
  if (!existsSync(join(HERE, "manifest.ts")) || readFileSync(join(HERE, "manifest.ts"), "utf8") !== manifest) {
    writeFileSync(join(HERE, "manifest.ts"), manifest);
  }
  writeFileSync(join(HERE, "css-urls.json.tmp"), JSON.stringify(liveCssUrls));
  renameSync(join(HERE, "css-urls.json.tmp"), join(HERE, "css-urls.json"));
  phase("client bundle: outputs written");
  // The session's bundler idles until the next edit: hand the outputs' native
  // memory back now (Linux, 3k-module app: idle ~570MB -> ~455MB). An
  // engine's outputs stay its own.
  if (!live) await freeRolldownOutputs(APP, ["vite", "@tanstack/react-start"], result.output);
  process.stderr.write(`${OJ}${_ojTTY ? "" : ":"} client bundled (${result.output.length} files)\n`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) await main();
