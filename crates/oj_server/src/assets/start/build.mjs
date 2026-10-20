// SPDX-License-Identifier: MIT

import { existsSync, readFileSync, writeFileSync, mkdirSync, cpSync, rmSync } from "node:fs";
import { dirname, join, resolve, relative } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import {
  importPkg,
  importOjRolldown,
  importOjRolldownParseAst,
  moduleParser,
  viteEnvDefine,
  environmentDefines,
  jsxTransformOptions,
  ssrExternalRule,
} from "./resolve-pkg.mjs";
import {
  appAliasPlugin,
  assetsPlugin,
  clientExternal,
  clientChunkPolicy,
  makeVitePlugins,
  nodeBuiltinShims,
  workspaceRoot,
  contentHashEmitter,
  viteOnLog,
} from "./rolldown-assets.mjs";
import { loadPluginContainer } from "./vite-plugin-bridge.mjs";
import { transformGlob, transformImportMetaUrl, depImportMetaUrlPlugin, bundlerResolve } from "./glob-transform.mjs";
import { rewriteClientServerFns, rewriteServerServerFns } from "./server-fns.mjs";
import { cloudflareEnvironment, cloudflareWorkerPlugin, workerOutDir, CLOUDFLARE_WORKER_ENTRY } from "./cf-build.mjs";

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

async function main() {
  const APP = process.env.OJ_APP_ROOT ?? process.cwd();
  // Set by `oj build` per Vite's NODE_ENV rule (shell wins, else .env NODE_ENV=development, else production).
  const NODE_ENV = process.env.NODE_ENV || "production";
  const MODE = process.env.OJ_MODE || "production";
  const PROCESS_ENV_JSON = JSON.stringify({ NODE_ENV, TSS_SERVER_FN_BASE: "/_serverFn/" });
  // The config's `define` map (OJ_DEFINE from oj), as Vite's define plugin applies it.
  const USER_DEFINE = (() => {
    try {
      return JSON.parse(process.env.OJ_DEFINE || "{}") || {};
    } catch {
      return {};
    }
  })();
  // Client export conditions from `resolve.conditions` (OJ_CLIENT_CONDITIONS from
  // oj, production-flavored for the build), as Vite's client environment.
  const CLIENT_CONDITIONS = (() => {
    try {
      const v = JSON.parse(process.env.OJ_CLIENT_CONDITIONS || "null");
      if (Array.isArray(v) && v.every((c) => typeof c === "string") && v.length) return v;
    } catch {}
    return ["browser", "module", "import", NODE_ENV === "production" ? "production" : "development"];
  })();
  const { build } = await importOjRolldown(APP, ["vite", "@tanstack/react-start"]);
  const parseAst = await importOjRolldownParseAst(APP, ["vite", "@tanstack/react-start"]);
  const _ojTTY = process.stderr.isTTY && !process.env.NO_COLOR;
  const OJ = _ojTTY ? "\x1b[48;2;255;255;255m\x1b[1;38;2;42;51;212m oj \x1b[0m" : "oj";

  const HERE = dirname(fileURLToPath(import.meta.url));
  // Resolved by `oj build` from `--out` / `build.outDir`, `base`, `build.sourcemap`
  // and `build.minify`, the options Vite applies to a Start app's client bundle.
  const DIST = process.env.OJ_OUT_DIR ? resolve(process.env.OJ_OUT_DIR) : join(APP, "dist");
  if (DIST === resolve(APP)) throw new Error(`build.outDir ${DIST} is the project root; refusing to empty it`);
  const BASE = process.env.OJ_BASE || "/";
  const SOURCEMAP = (() => {
    const v = process.env.OJ_SOURCEMAP;
    return v === "true" ? true : v === "inline" || v === "hidden" ? v : false;
  })();
  const MINIFY = process.env.OJ_MINIFY !== "false";
  const CLIENT = join(DIST, "client");
  const WORKSPACE = workspaceRoot(APP);

  const WORKER = [
    'import handler from "./server-bundle.mjs";',
    "export default { async fetch(request, env, ctx) { return handler.fetch(request); } };",
    "",
  ].join("\n");

  const SERVER = [
    'import { createServer } from "node:http";',
    'import { readFile } from "node:fs/promises";',
    'import { join, extname, dirname } from "node:path";',
    'import { fileURLToPath } from "node:url";',
    'import { register } from "node:module";',
    'import { Readable } from "node:stream";',
    'import { pipeline } from "node:stream/promises";',
    'register("./cf-loader.mjs", import.meta.url);',
    'const handler = (await import("./server-bundle.mjs")).default;',
    "const HERE = dirname(fileURLToPath(import.meta.url));",
    'const CLIENT = join(HERE, "client");',
    `const BASE = ${JSON.stringify(BASE)};`,
    "const PORT = process.env.PORT || 3000;",
    'const MIME = { ".html":"text/html; charset=utf-8", ".js":"text/javascript", ".css":"text/css", ".json":"application/json", ".wasm":"application/wasm", ".ico":"image/x-icon", ".png":"image/png", ".svg":"image/svg+xml", ".webp":"image/webp", ".avif":"image/avif", ".gif":"image/gif", ".jpg":"image/jpeg", ".jpeg":"image/jpeg", ".woff2":"font/woff2", ".woff":"font/woff", ".ttf":"font/ttf", ".otf":"font/otf", ".txt":"text/plain; charset=utf-8", ".xml":"application/xml", ".webmanifest":"application/manifest+json" };',
    "// Request bodies stream into the handler and responses stream out (Start",
    "// streams its dehydrated data into the HTML; binary bodies stay bytes), and",
    "// every set-cookie header survives, as in the dev runner.",
    "createServer(async (req, res) => {",
    '  const url = new URL(req.url, "http://" + (req.headers.host || "localhost"));',
    '  if (req.method === "GET") {',
    '    const rel = (BASE !== "/" && url.pathname.startsWith(BASE) ? url.pathname.slice(BASE.length) : url.pathname).replace(/^\\/+/, "");',
    '    if (!rel.startsWith("_serverFn/")) {',
    "      const candidates = [];",
    "      if (rel) candidates.push(rel);",
    '      candidates.push((rel ? rel.replace(/\\/+$/, "") + "/" : "") + "index.html");',
    "      for (const c of candidates) {",
    '        try { const bytes = await readFile(join(CLIENT, c)); res.writeHead(200, { "content-type": MIME[extname(c)] || "application/octet-stream" }); res.end(bytes); return; } catch {}',
    "      }",
    "    }",
    "  }",
    "  const headers = new Headers();",
    "  for (const [k, v] of Object.entries(req.headers)) {",
    "    if (v == null) continue;",
    "    if (Array.isArray(v)) for (const x of v) headers.append(k, x); else headers.set(k, v);",
    "  }",
    "  const init = { method: req.method, headers };",
    '  if (req.method !== "GET" && req.method !== "HEAD") { init.body = Readable.toWeb(req); init.duplex = "half"; }',
    '  if (url.pathname.endsWith("/index.html")) url.pathname = url.pathname.slice(0, -10);',
    "  try {",
    "    const response = await handler.fetch(new Request(url.href, init));",
    "    const out = {};",
    '    response.headers.forEach((v, k) => { if (k !== "set-cookie" && k !== "content-length") out[k] = v; });',
    "    const cookies = response.headers.getSetCookie?.() ?? [];",
    '    if (cookies.length) out["set-cookie"] = cookies;',
    "    res.writeHead(response.status, out);",
    "    if (response.body) await pipeline(Readable.fromWeb(response.body), res); else res.end();",
    "  } catch (e) {",
    '    if (!res.headersSent) res.writeHead(500, { "content-type": "text/plain; charset=utf-8" });',
    "    res.end(String((e && e.stack) || e));",
    "  }",
    '}).listen(PORT, () => console.log("oj tanstack server on http://localhost:" + PORT));',
    "",
  ].join("\n");

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

  const userTs = (id) => !id.includes("/node_modules/") && !id.startsWith("\0") && /\.(tsx?|jsx?|mjs)$/.test(id);

  const clientFnPlugin = {
    name: "server-fn-client",
    async transform(code, id) {
      if (!userTs(id)) return null;
      const glob = transformGlob(await transformImportMetaUrl(code, id, bundlerResolve(this)), id);
      return (
        rewriteClientServerFns(glob, relative(APP, id), moduleParser(parseAst, id)) ?? (glob === code ? null : glob)
      );
    },
  };

  const serverFnPlugin = {
    name: "server-fn-server",
    transform(code, id) {
      if (!userTs(id)) return null;
      const glob = transformGlob(code, id);
      return rewriteServerServerFns(glob, relative(APP, id)) ?? (glob === code ? null : glob);
    },
  };

  rmSync(DIST, { recursive: true, force: true });
  mkdirSync(CLIENT, { recursive: true });

  const compileCss = await (async () => {
    try {
      const postcss = (await importPkg(APP, "postcss", [])).default ?? (await importPkg(APP, "postcss", []));
      const twMod = await importPkg(APP, "@tailwindcss/postcss", ["tailwindcss"]);
      const tailwind = twMod.default ?? twMod;
      return async (from, src) => (await postcss([tailwind()]).process(src, { from })).css;
    } catch {}
    try {
      const anchors = ["@tailwindcss/vite", "@tailwindcss/postcss", "tailwindcss"];
      const tw = await importPkg(APP, "@tailwindcss/node", anchors);
      const oxide = await importPkg(APP, "@tailwindcss/oxide", anchors);
      return async (from, src) => {
        // As @tailwindcss/postcss: `@source` resolves from the sheet's dir and
        // the scan covers `source(...)` plus every `@source`.
        const compiler = await tw.compile(src, { base: dirname(from), from, onDependency: () => {} });
        const root =
          compiler.root === "none"
            ? []
            : compiler.root == null
              ? [{ base: APP, pattern: "**/*", negated: false }]
              : [{ ...compiler.root, negated: false }];
        const sources = compiler.sources ?? (compiler.globs ?? []).map((g) => ({ ...g, negated: false }));
        const scanner = new oxide.Scanner({ sources: root.concat(sources) });
        return compiler.build(scanner.scan());
      };
    } catch {}
    return null;
  })();
  const CSS_MINIFY = process.env.OJ_START_CSS_MINIFY ? JSON.parse(process.env.OJ_START_CSS_MINIFY) : null;
  const emit = contentHashEmitter(CLIENT, compileCss, BASE, CSS_MINIFY);
  const clientContainer = await loadPluginContainer(APP, { command: "build", mode: MODE, environment: "client" });
  let serverContainer = await loadPluginContainer(APP, { command: "build", mode: MODE, environment: "ssr" });
  // With @cloudflare/vite-plugin the server build is the Worker environment the
  // plugin declared (its name comes from the wrangler config, or the plugin's
  // `viteEnvironment.name`); the plugin's hooks must see that environment.
  const cfEnv = cloudflareEnvironment(serverContainer?.config);
  if (cfEnv && cfEnv.name !== "ssr") {
    serverContainer = await loadPluginContainer(APP, { command: "build", mode: MODE, environment: cfEnv.name });
  }

  const clientAlias = {
    "#tanstack-router-entry": routerEntry(),
    "#tanstack-start-entry": startEntry(),
    "#tanstack-start-plugin-adapters": join(HERE, "plugin-adapters.ts"),
    "tanstack-start-manifest:v": join(HERE, "manifest.ts"),
    "tanstack-start-injected-head-scripts:v": join(HERE, "injected-head-scripts.ts"),
    "@tanstack/start-fn-stubs": join(HERE, "fn-stubs.mjs"),
  };

  const clientDefine = {
    ...USER_DEFINE,
    ...environmentDefines("client"),
    ...(clientContainer?.defines?.() ?? {}),
    "process.env": PROCESS_ENV_JSON,
    global: "globalThis",
    ...viteEnvDefine({ ssr: false, mode: MODE, base: BASE }),
  };

  // `?worker` / `?worker&url` in a build: Vite's bundleWorkerEntry bundles the
  // worker entry on its own (one ES file, the client's transform and define)
  // and emits it as an asset.
  emit.setWorkerBundler(async (file) => {
    const out = await build({
      input: file,
      platform: "browser",
      transform: { jsx: jsxTransformOptions(NODE_ENV !== "production"), define: clientDefine },
      resolve: { conditionNames: CLIENT_CONDITIONS, alias: clientAlias },
      ...clientExternal(clientContainer?.config),
      onLog: viteOnLog,
      // Vite's bundleWorkerEntry runs the worker plugin pipeline, so imports
      // inside a worker entry resolve exactly as in its importer: the app's
      // plugins (a Cloudflare-shaped `#subpath` rule, `virtual:` modules) and
      // the Start aliases included. Module workers are always ES here, as in
      // oj's dev serving; a `worker.format: "iife"` config has no oj analog.
      plugins: [
        appAliasPlugin(clientContainer?.config),
        makeVitePlugins({ container: clientContainer, appRoot: APP, mode: "prod", emit, lifecycle: false }),
        depImportMetaUrlPlugin({ include: /\.[cm]?[jt]sx?$/, self: file }),
        assetsPlugin({ mode: "prod", server: false, emit }),
        nodeBuiltinShims({ production: true }),
      ],
      output: { format: "esm", minify: MINIFY, codeSplitting: false },
      write: false,
    });
    return out.output.find((o) => o.type === "chunk" && o.isEntry).code;
  });

  const clientPolicy = clientChunkPolicy(clientContainer?.config);
  const client = await build({
    input: { client: join(HERE, "client-entry.tsx") },
    platform: "browser",
    ...clientPolicy.input,
    transform: {
      jsx: jsxTransformOptions(NODE_ENV !== "production"),
      define: clientDefine,
    },
    resolve: { conditionNames: CLIENT_CONDITIONS, alias: clientAlias },
    ...clientExternal(clientContainer?.config),
    onLog: viteOnLog,
    plugins: [
      appAliasPlugin(clientContainer?.config),
      makeVitePlugins({ container: clientContainer, appRoot: APP, mode: "prod", emit }),
      {
        name: "oj-client-render-chunk",
        renderChunk: (code, chunk) => clientContainer?.renderChunk(code, chunk) ?? null,
      },
      clientFnPlugin,
      depImportMetaUrlPlugin(),
      assetsPlugin({ mode: "prod", server: false, emit }),
      nodeBuiltinShims({ production: true }),
    ],
    output: {
      dir: CLIENT,
      format: "esm",
      minify: MINIFY,
      sourcemap: SOURCEMAP,
      entryFileNames: "assets/[name]-[hash].js",
      chunkFileNames: "assets/[name]-[hash].js",
      assetFileNames: "assets/[name]-[hash][extname]",
      banner: `globalThis.process=globalThis.process||{env:${PROCESS_ENV_JSON}};globalThis.global=globalThis.global||globalThis;`,
      ...clientPolicy.output,
    },
  });
  const entryChunk = client.output.find((o) => o.type === "chunk" && o.isEntry);
  const clientUrl = BASE + entryChunk.fileName;

  if (clientContainer) {
    const bundle = Object.fromEntries(client.output.map((output) => [output.fileName, output]));
    const originalCode = new Map(
      client.output.filter((output) => output.type === "chunk").map((output) => [output.fileName, output.code]),
    );
    await clientContainer.generateBundle(({ type, fileName, source }) => {
      if (type !== "asset" || !fileName || source == null) return;
      const outFile = join(CLIENT, fileName);
      mkdirSync(dirname(outFile), { recursive: true });
      writeFileSync(outFile, source);
    }, bundle);
    for (const output of Object.values(bundle)) {
      if (output.type === "chunk" && output.code !== originalCode.get(output.fileName)) {
        writeFileSync(join(CLIENT, output.fileName), output.code);
      }
    }
    await clientContainer.writeBundle(bundle);
  }

  // The SERVER bundle bakes this rewrite in (serverAlias maps the manifest
  // virtual to the same file, and the server build runs after this write), so
  // it must serve the same shape union as the static shims: 1.168+ reads
  // per-route `scripts`/`css`, 1.167 and earlier read the top-level
  // `clientEntry` (`import(undefined)` without it) and css from per-route
  // `assets`. Each version copies only the fields it knows.
  const rootManifest = {
    preloads: [clientUrl],
    css: emit.cssUrls(),
    scripts: [{ attrs: { type: "module", async: true, src: clientUrl } }],
    assets: emit.cssUrls().map((href) => ({ tag: "link", attrs: { rel: "stylesheet", href } })),
  };
  writeFileSync(
    join(HERE, "manifest.ts"),
    `export const tsrStartManifest = () => (${JSON.stringify({ routes: { __root__: rootManifest }, clientEntry: clientUrl })});\n`,
  );

  const serverDefine = {
    ...USER_DEFINE,
    ...environmentDefines("ssr"),
    ...(serverContainer?.defines?.() ?? {}),
    "process.env.NODE_ENV": JSON.stringify(NODE_ENV),
    "process.env.TSS_SERVER_FN_BASE": '"/_serverFn/"',
    ...viteEnvDefine({ ssr: true, mode: MODE, base: BASE }),
  };
  // The bundle's entry is the app's own Start server entry when configured
  // (`tanstackStart({ server: { entry } })`, published by the plugin as the
  // `virtual:tanstack-start-server-entry` alias and exported by oj as
  // OJ_START_SERVER_ENTRY), else oj's; either way `@tanstack/react-start/server-entry`
  // inside it is oj's handler, so a wrapping entry composes as it does under Vite.
  const OJ_SERVER_ENTRY = join(HERE, "server-entry.tsx");
  const SERVER_ENTRY = process.env.OJ_START_SERVER_ENTRY || OJ_SERVER_ENTRY;
  const serverAlias = {
    ...clientAlias,
    "#tanstack-start-server-fn-resolver": join(HERE, "server-fn-resolver.mjs"),
    "@tanstack/react-start/server-entry": OJ_SERVER_ENTRY,
  };
  const serverPlugins = () => [
    appAliasPlugin((serverContainer ?? clientContainer)?.config),
    makeVitePlugins({ container: serverContainer, fallback: clientContainer, appRoot: APP, mode: "prod", emit }),
    {
      name: "oj-server-render-chunk",
      renderChunk: (code, chunk) => serverContainer?.renderChunk(code, chunk) ?? null,
    },
    serverFnPlugin,
    assetsPlugin({ mode: "prod", server: true, emit }),
  ];

  let workerDir = null;
  if (cfEnv) {
    // The Worker environment build, as `vite build` runs it for the Cloudflare
    // plugin: dist/<environment>/index.js bundled for workerd (neutral platform,
    // the runtime's built-ins external, no Node server wrapper), the plugin's
    // virtual Worker entry wrapping oj's server entry, and the plugin's
    // generateBundle/writeBundle emitting wrangler.json and the deploy config.
    workerDir = workerOutDir(cfEnv, APP, DIST, serverContainer.config);
    const serverEntry = SERVER_ENTRY;
    const worker = await build({
      input: { index: CLOUDFLARE_WORKER_ENTRY },
      platform: "neutral",
      transform: {
        jsx: jsxTransformOptions(NODE_ENV !== "production"),
        ...(cfEnv.target ? { target: cfEnv.target } : {}),
        define: {
          ...serverDefine,
          // The environment's own define (the plugin's process.env replacements).
          ...serverContainer.defines(),
          "import.meta.hot": "undefined",
        },
      },
      resolve: {
        conditionNames: [...cfEnv.conditions, "import", NODE_ENV === "production" ? "production" : "development"],
        alias: {
          ...serverAlias,
          "@cloudflare/vite-plugin/server": join(HERE, "cf-server-worker.mjs"),
        },
      },
      plugins: [
        cloudflareWorkerPlugin({ container: serverContainer, env: cfEnv, serverEntry }),
        ...serverPlugins(),
        ...cfEnv.rolldownPlugins,
      ],
      output: {
        dir: workerDir,
        format: "esm",
        minify: MINIFY,
        sourcemap: SOURCEMAP,
        entryFileNames: "[name].js",
        chunkFileNames: "assets/[name]-[hash].js",
        assetFileNames: "assets/[name]-[hash][extname]",
      },
    });
    const bundle = Object.fromEntries(worker.output.map((output) => [output.fileName, output]));
    const originalCode = new Map(worker.output.filter((o) => o.type === "chunk").map((o) => [o.fileName, o.code]));
    await serverContainer.generateBundle(({ type, fileName, source }) => {
      if (type !== "asset" || !fileName || source == null) return;
      const outFile = join(workerDir, fileName);
      mkdirSync(dirname(outFile), { recursive: true });
      writeFileSync(outFile, source);
    }, bundle);
    for (const output of Object.values(bundle)) {
      if (output.type === "chunk" && output.code !== originalCode.get(output.fileName)) {
        writeFileSync(join(workerDir, output.fileName), output.code);
      }
    }
    await serverContainer.writeBundle(bundle);
  } else {
    const server = await build({
      input: { "server-bundle": SERVER_ENTRY },
      platform: "node",
      // Vite's `ssr.external`: those dependencies stay bare imports of the bundle.
      external: ssrExternalRule(APP),
      transform: {
        jsx: jsxTransformOptions(NODE_ENV !== "production"),
        define: serverDefine,
      },
      resolve: {
        alias: {
          ...serverAlias,
          "@cloudflare/vite-plugin/server": join(HERE, "cf-server.mjs"),
        },
      },
      plugins: serverPlugins(),
      output: {
        dir: DIST,
        format: "esm",
        minify: MINIFY,
        sourcemap: SOURCEMAP,
        entryFileNames: "[name].mjs",
        chunkFileNames: "chunks/[name]-[hash].mjs",
        banner:
          "import { createRequire as ___cr } from 'node:module'; const require = ___cr(import.meta.url || 'file:///worker.js');",
      },
    });
    // Vite runs writeBundle for the server environment as well, once its files exist.
    if (serverContainer) {
      await serverContainer.writeBundle(Object.fromEntries(server.output.map((output) => [output.fileName, output])));
    }

    writeFileSync(join(DIST, "server.mjs"), SERVER);
    writeFileSync(join(DIST, "worker.mjs"), WORKER);
    cpSync(join(HERE, "cf-server.mjs"), join(DIST, "cf-server.mjs"));
    writeFileSync(
      join(DIST, "cf-loader.mjs"),
      "export async function resolve(spec, ctx, next) {\n" +
        '  if (spec === "@cloudflare/vite-plugin/server")\n' +
        '    return { url: new URL("./cf-server.mjs", import.meta.url).href, shortCircuit: true };\n' +
        "  return next(spec, ctx);\n" +
        "}\n",
    );
  }

  if (clientContainer?.publicDir !== false) {
    const publicDir = resolve(APP, clientContainer?.publicDir ?? "public");
    if (existsSync(publicDir)) cpSync(publicDir, CLIENT, { recursive: true });
  }

  const prerender = (process.env.OJ_PRERENDER || "")
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
  if (prerender.length && cfEnv) {
    // The Worker bundle imports the runtime's own modules; it does not run under Node.
    process.stderr.write(
      `${OJ}${_ojTTY ? "" : ":"} prerender skipped: the Cloudflare Worker bundle only runs in workerd\n`,
    );
  } else if (prerender.length) {
    const handler = (await import(pathToFileURL(join(DIST, "server-bundle.mjs")).href)).default;
    for (const route of prerender) {
      const res = await handler.fetch(new Request("http://localhost" + route));
      const html = await res.text();
      const rel = route === "/" ? "index.html" : `${route.replace(/^\/+/, "").replace(/\/+$/, "")}/index.html`;
      const outFile = join(CLIENT, rel);
      mkdirSync(dirname(outFile), { recursive: true });
      writeFileSync(outFile, html);
    }
    process.stderr.write(`${OJ}${_ojTTY ? "" : ":"} prerendered ${prerender.length} route(s)\n`);
  }

  await clientContainer?.closeBundle();
  await serverContainer?.closeBundle();

  process.stderr.write(`${OJ}${_ojTTY ? "" : ":"} built dist (client ${clientUrl})\n`);
  if (workerDir) {
    process.stderr.write(
      `${OJ}${_ojTTY ? "" : ":"} cloudflare worker "${cfEnv.name}" -> ${relative(APP, workerDir)}/index.js (deploy: wrangler deploy)\n`,
    );
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) await main();
