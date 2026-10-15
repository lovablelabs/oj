// SPDX-License-Identifier: MIT

import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, isAbsolute, join, resolve, relative } from "node:path";
import { fileURLToPath } from "node:url";
import {
  freeRolldownOutputs,
  importPkg,
  importOjRolldown,
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
  viteOnLog,
  workspaceRoot,
} from "./rolldown-assets.mjs";
import { loadPluginContainer } from "./vite-plugin-bridge.mjs";
import { transformGlob } from "./glob-transform.mjs";
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
  const container = await containerLoad?.catch(() => null);
  if (container?.buildEnd) await container.buildEnd();
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

  function rewriteServerFns(code, id) {
    if (!code.includes("createServerFn")) return null;
    const rel = relative(APP, id);
    const re = /(?:export\s+)?const\s+([A-Za-z_$][\w$]*)\s*=\s*createServerFn\b[\s\S]*?\.handler\s*\(/g;
    const edits = [];
    let m;
    while ((m = re.exec(code))) {
      const open = m.index + m[0].length;
      let depth = 1;
      let i = open;
      for (; i < code.length && depth > 0; i++) {
        if (code[i] === "(") depth++;
        else if (code[i] === ")") depth--;
      }
      edits.push({ name: m[1], open, close: i - 1 });
    }
    if (!edits.length) return null;
    let out = code;
    for (const e of edits.reverse()) {
      const id2 = Buffer.from(`${rel}#${e.name}`).toString("base64url");
      out = out.slice(0, e.open) + `createClientRpc(${JSON.stringify(id2)})` + out.slice(e.close);
    }
    // On the first line, not above it: the rewrite must not shift source lines.
    return `import { createClientRpc } from "@tanstack/react-start/client-rpc"; ${out}`;
  }

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
  // Every import.meta.glob scan base this bundle expanded; the dev rebundle
  // gate rebundles for a file CREATED under one (the glob's expansion grows),
  // and for no other create, mirroring Vite's add handling.
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
        out = transformGlob(out, id, undefined, globScopes);
        const rpc = rewriteServerFns(out, id);
        if (rpc != null) return rpc;
        return out === code ? null : out;
      },
    },
  };

  const chunkPolicy = devSafeChunkPolicy(clientChunkPolicy(container?.config));
  phase("client bundle: build starting");
  // The last failure, as Vite's bundled dev keeps lastBuildError: the dev
  // server replays it to every page as the error overlay until a bundle
  // succeeds.
  const errorFile = join(HERE, "client-error.json");
  let result;
  try {
    result = await build({
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
          styleKey: { css: { epoch: process.env.OJ_START_STYLE_EPOCH ?? "0" }, inline: { bundle: String(++bundles) } },
        }),
        serverFnClient,
        nodeBuiltinShims({ production: false }),
      ],
      output: {
        format: "esm",
        banner:
          `globalThis.process=globalThis.process||{env:{NODE_ENV:${JSON.stringify(NODE_ENV)},TSS_SERVER_FN_BASE:${JSON.stringify(SERVER_FN_BASE)}}};` +
          "globalThis.global=globalThis.global||globalThis;",
        ...chunkPolicy.output,
      },
      write: false,
    });
  } catch (error) {
    writeFileSync(errorFile, JSON.stringify(buildErrorPayload(error)));
    throw error;
  }
  rmSync(errorFile, { force: true });

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
    ...visitedIds,
    ...(container?.configDependencies ?? []),
    ...(container?.watchFiles ?? []),
  ]);
  for (const o of result.output) {
    if (o.type !== "chunk") continue;
    for (const id of Object.keys(o.modules ?? {})) closure.add(id);
  }
  const closureFiles = [...closure]
    // An asset module's id is virtual (\0oj-css:/abs/x.css and friends), but
    // its FILE is an input: raw/inline embed its bytes, css lands in
    // css-urls.json. The closure must carry the file, or an edit to it never
    // rebundles.
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
  writeFileSync(join(HERE, "glob-scopes.json"), JSON.stringify([...globScopes].sort()));

  phase("client bundle: closure written");
  const devManifest = {
    routes: {
      __root__: {
        preloads: ["/@oj-start/client-entry.js"],
        css: cssUrls,
        scripts: [{ attrs: { type: "module", async: true, src: "/@oj-start/client-entry.js" } }],
      },
    },
  };
  writeFileSync(join(HERE, "manifest.ts"), `export const tsrStartManifest = () => (${JSON.stringify(devManifest)});\n`);
  writeFileSync(join(HERE, "css-urls.json.tmp"), JSON.stringify(cssUrls));
  renameSync(join(HERE, "css-urls.json.tmp"), join(HERE, "css-urls.json"));
  phase("client bundle: outputs written");
  // The session's bundler idles until the next edit: hand the outputs' native
  // memory back now (Linux, 3k-module app: idle ~570MB -> ~455MB).
  await freeRolldownOutputs(APP, ["vite", "@tanstack/react-start"], result.output);
  process.stderr.write(`${OJ}${_ojTTY ? "" : ":"} client bundled (${result.output.length} files)\n`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) await main();
