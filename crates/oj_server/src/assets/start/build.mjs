// SPDX-License-Identifier: MIT

import { existsSync, readFileSync, writeFileSync, mkdirSync, cpSync, rmSync } from "node:fs";
import { dirname, join, resolve, relative } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { importPkg, viteEnvDefine, environmentDefines, jsxTransformOptions, ssrExternalRule } from "./resolve-pkg.mjs";
import {
  assetsPlugin, makeVitePlugins, nodeBuiltinShims, workspaceRoot, contentHashEmitter,
} from "./rolldown-assets.mjs";
import { loadPluginContainer } from "./vite-plugin-bridge.mjs";
import { transformGlob } from "./glob-transform.mjs";
import { cloudflareEnvironment, cloudflareWorkerPlugin, workerOutDir, CLOUDFLARE_WORKER_ENTRY } from "./cf-build.mjs";

// Runs on oj's embedded engine (`ScriptEngine`): the values that used to be
// spawn env arrive as `env`; `node <script>` still works for tests.
export async function run(env = null) {
  if (env) for (const [k, v] of Object.entries(env)) process.env[k] = v;
  // The node spawn ran with cwd = the app root; cwd-relative plugin filters
  // (svgr's createFilter globs) depend on it.
  if (env && env.OJ_APP_ROOT) {
    try { process.chdir(env.OJ_APP_ROOT); } catch {}
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
  try { return JSON.parse(process.env.OJ_DEFINE || "{}") || {}; } catch { return {}; }
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
const { build } = await importPkg(APP, "rolldown", ["vite", "@tanstack/react-start"]);
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
const sfid = (rel, name) => Buffer.from(`${rel}#${name}`).toString("base64url");

const WORKER = [
  'import handler from "./server-bundle.mjs";',
  'export default { async fetch(request, env, ctx) { return handler.fetch(request); } };',
  '',
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
  'const HERE = dirname(fileURLToPath(import.meta.url));',
  'const CLIENT = join(HERE, "client");',
  `const BASE = ${JSON.stringify(BASE)};`,
  'const PORT = process.env.PORT || 3000;',
  'const MIME = { ".html":"text/html; charset=utf-8", ".js":"text/javascript", ".css":"text/css", ".json":"application/json", ".wasm":"application/wasm", ".ico":"image/x-icon", ".png":"image/png", ".svg":"image/svg+xml", ".webp":"image/webp", ".avif":"image/avif", ".gif":"image/gif", ".jpg":"image/jpeg", ".jpeg":"image/jpeg", ".woff2":"font/woff2", ".woff":"font/woff", ".ttf":"font/ttf", ".otf":"font/otf", ".txt":"text/plain; charset=utf-8", ".xml":"application/xml", ".webmanifest":"application/manifest+json" };',
  '// Request bodies stream into the handler and responses stream out (Start',
  '// streams its dehydrated data into the HTML; binary bodies stay bytes), and',
  '// every set-cookie header survives, as in the dev runner.',
  'createServer(async (req, res) => {',
  '  const url = new URL(req.url, "http://" + (req.headers.host || "localhost"));',
  '  if (req.method === "GET") {',
  '    const rel = (BASE !== "/" && url.pathname.startsWith(BASE) ? url.pathname.slice(BASE.length) : url.pathname).replace(/^\\/+/, "");',
  '    if (!rel.startsWith("_serverFn/")) {',
  '      const candidates = [];',
  '      if (rel) candidates.push(rel);',
  '      candidates.push((rel ? rel.replace(/\\/+$/, "") + "/" : "") + "index.html");',
  '      for (const c of candidates) {',
  '        try { const bytes = await readFile(join(CLIENT, c)); res.writeHead(200, { "content-type": MIME[extname(c)] || "application/octet-stream" }); res.end(bytes); return; } catch {}',
  '      }',
  '    }',
  '  }',
  '  const headers = new Headers();',
  '  for (const [k, v] of Object.entries(req.headers)) {',
  '    if (v == null) continue;',
  '    if (Array.isArray(v)) for (const x of v) headers.append(k, x); else headers.set(k, v);',
  '  }',
  '  const init = { method: req.method, headers };',
  '  if (req.method !== "GET" && req.method !== "HEAD") { init.body = Readable.toWeb(req); init.duplex = "half"; }',
  '  if (url.pathname.endsWith("/index.html")) url.pathname = url.pathname.slice(0, -10);',
  '  try {',
  '    const response = await handler.fetch(new Request(url.href, init));',
  '    const out = {};',
  '    response.headers.forEach((v, k) => { if (k !== "set-cookie" && k !== "content-length") out[k] = v; });',
  '    const cookies = response.headers.getSetCookie?.() ?? [];',
  '    if (cookies.length) out["set-cookie"] = cookies;',
  '    res.writeHead(response.status, out);',
  '    if (response.body) await pipeline(Readable.fromWeb(response.body), res); else res.end();',
  '  } catch (e) {',
  '    if (!res.headersSent) res.writeHead(500, { "content-type": "text/plain; charset=utf-8" });',
  '    res.end(String((e && e.stack) || e));',
  '  }',
  '}).listen(PORT, () => console.log("oj tanstack server on http://localhost:" + PORT));',
  '',
].join("\n");

// The app's package.json "imports" wins over the src/router convention, the
// way it does in the SSR loader: the framework makes this path configurable
// (`router.entry`), and an app that moved it has to be able to say so.
function declaredRouterEntry() {
  try {
    const target = JSON.parse(readFileSync(resolve(APP, "package.json"), "utf8"))
      .imports?.["#tanstack-router-entry"];
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

function serverFns(code) {
  const re = /(?:export\s+)?const\s+([A-Za-z_$][\w$]*)\s*=\s*createServerFn\b[\s\S]*?\.handler\s*\(/g;
  const out = [];
  let m;
  while ((m = re.exec(code))) {
    let depth = 1, i = m.index + m[0].length;
    for (; i < code.length && depth > 0; i++) {
      if (code[i] === "(") depth++;
      else if (code[i] === ")") depth--;
    }
    out.push({ name: m[1], open: m.index + m[0].length, close: i - 1 });
  }
  return out;
}

const userTs = (id) => !id.includes("/node_modules/") && !id.startsWith("\0") && /\.(tsx?|jsx?|mjs)$/.test(id);

const clientFnPlugin = {
  name: "server-fn-client",
  transform(code, id) {
    if (!userTs(id)) return null;
    const glob = transformGlob(code, id);
    if (!glob.includes("createServerFn")) return glob === code ? null : glob;
    const rel = relative(APP, id);
    const fns = serverFns(glob);
    if (!fns.length) return glob === code ? null : glob;
    let out = glob;
    for (const f of fns.reverse()) {
      out = out.slice(0, f.open) + `createClientRpc(${JSON.stringify(sfid(rel, f.name))})` + out.slice(f.close);
    }
    return `import { createClientRpc } from "@tanstack/react-start/client-rpc";\n${out}`;
  },
};

const serverFnPlugin = {
  name: "server-fn-server",
  transform(code, id) {
    if (!userTs(id)) return null;
    const glob = transformGlob(code, id);
    if (!glob.includes("createServerFn")) return glob === code ? null : glob;
    const rel = relative(APP, id);
    const re =
      /(^|[\n;])([ \t]*)((?:export\s+)?const\s+([A-Za-z_$][\w$]*)\s*=\s*createServerFn\b[\s\S]*?\.handler\s*\()/g;
    let changed = false;
    const out = glob.replace(re, (_m, pre, indent, decl, name) => {
      changed = true;
      const meta = `{ id: ${JSON.stringify(sfid(rel, name))}, name: ${JSON.stringify(name)}, filename: ${JSON.stringify(rel)} }`;
      return `${pre}${indent}export const ${name}_createServerFn_handler = createServerRpc(${meta}, (opts) => ${name}.__executeServer(opts));\n${indent}${decl}${name}_createServerFn_handler, `;
    });
    if (!changed) return glob === code ? null : glob;
    return `import { createServerRpc } from "@tanstack/react-start/server-rpc";\n${out}`;
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
      const compiler = await tw.compile(src, { base: APP, from, onDependency: () => {} });
      const scanner = new oxide.Scanner({ sources: [{ base: APP, pattern: "**/*", negated: false }] });
      return compiler.build(scanner.scan());
    };
  } catch {}
  return null;
})();
const emit = contentHashEmitter(CLIENT, compileCss, BASE);
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
  "#tanstack-start-entry": join(HERE, "start-entry.ts"),
  "#tanstack-start-plugin-adapters": join(HERE, "plugin-adapters.ts"),
  "tanstack-start-manifest:v": join(HERE, "manifest.ts"),
  "@tanstack/start-fn-stubs": join(HERE, "fn-stubs.mjs"),
};

const client = await build({
  input: { client: join(HERE, "client-entry.tsx") },
  platform: "browser",
  transform: {
    jsx: jsxTransformOptions(NODE_ENV !== "production"),
    define: {
      ...USER_DEFINE,
      ...environmentDefines("client"),
      ...(clientContainer?.defines?.() ?? {}),
      "process.env": PROCESS_ENV_JSON,
      global: "globalThis", ...viteEnvDefine({ ssr: false, mode: MODE, base: BASE }),
    },
  },
  resolve: { conditionNames: CLIENT_CONDITIONS, alias: clientAlias },
  plugins: [
    makeVitePlugins({ container: clientContainer, appRoot: APP, mode: "prod", emit }),
    {
      name: "oj-client-render-chunk",
      renderChunk: (code, chunk) => clientContainer?.renderChunk(code, chunk) ?? null,
    },
    clientFnPlugin,
    assetsPlugin({ mode: "prod", server: false, emit }),
    nodeBuiltinShims,
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
  },
});
const entryChunk = client.output.find((o) => o.type === "chunk" && o.isEntry);
const clientUrl = BASE + entryChunk.fileName;

if (clientContainer) {
  const bundle = Object.fromEntries(client.output.map((output) => [output.fileName, output]));
  const originalCode = new Map(client.output
    .filter((output) => output.type === "chunk")
    .map((output) => [output.fileName, output.code]));
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

const rootManifest = {
  preloads: [clientUrl],
  css: emit.cssUrls(),
  scripts: [{ attrs: { type: "module", async: true, src: clientUrl } }],
};
writeFileSync(
  join(HERE, "manifest.ts"),
  `export const tsrStartManifest = () => (${JSON.stringify({ routes: { __root__: rootManifest } })});\n`,
);

const serverDefine = {
  ...USER_DEFINE,
  ...environmentDefines("ssr"),
  ...(serverContainer?.defines?.() ?? {}),
  "process.env.NODE_ENV": JSON.stringify(NODE_ENV), "process.env.TSS_SERVER_FN_BASE": '"/_serverFn/"',
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
      banner: "import { createRequire as ___cr } from 'node:module'; const require = ___cr(import.meta.url || 'file:///worker.js');",
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
    'export async function resolve(spec, ctx, next) {\n' +
      '  if (spec === "@cloudflare/vite-plugin/server")\n' +
      '    return { url: new URL("./cf-server.mjs", import.meta.url).href, shortCircuit: true };\n' +
      '  return next(spec, ctx);\n' +
      '}\n',
  );
}

if (clientContainer?.publicDir !== false) {
  const publicDir = resolve(APP, clientContainer?.publicDir ?? "public");
  if (existsSync(publicDir)) cpSync(publicDir, CLIENT, { recursive: true });
}

const prerender = (process.env.OJ_PRERENDER || "").split(",").map((s) => s.trim()).filter(Boolean);
if (prerender.length && cfEnv) {
  // The Worker bundle imports the runtime's own modules; it does not run under Node.
  process.stderr.write(`${OJ}${_ojTTY ? "" : ":"} prerender skipped: the Cloudflare Worker bundle only runs in workerd\n`);
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
  process.stderr.write(`${OJ}${_ojTTY ? "" : ":"} cloudflare worker "${cfEnv.name}" -> ${relative(APP, workerDir)}/index.js (deploy: wrangler deploy)\n`);
}
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) await main();
