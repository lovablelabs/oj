// SPDX-License-Identifier: MIT

import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, isAbsolute, join, resolve, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { importPkg, importOjRolldown, viteEnvDefine, environmentDefines, jsxTransformOptions } from "./resolve-pkg.mjs";
import { assetsPlugin, makeVitePlugins, nodeBuiltinShims, workspaceRoot } from "./rolldown-assets.mjs";
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
    try { process.chdir(env.OJ_APP_ROOT); } catch {}
  }
  return main();
}

async function main() {


const HERE = dirname(fileURLToPath(import.meta.url));
const APP = process.env.OJ_APP_ROOT ?? process.cwd();
const { build } = await importOjRolldown(APP, ["vite", "@tanstack/react-start"]);
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
  try { return JSON.parse(process.env.OJ_DEFINE || "{}") || {}; } catch { return {}; }
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

const container = await loadPluginContainer(APP, { command: "serve", mode: MODE, environment: "client" });

const cssUrls = [];

const visitedIds = new Set();
const closureRecorder = {
  name: "oj-closure-recorder",
  transform(code, id) {
    visitedIds.add(id);
    return null;
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
      out = transformGlob(out, id);
      const rpc = rewriteServerFns(out, id);
      if (rpc != null) return rpc;
      return out === code ? null : out;
    },
  },
};

const result = await build({
  input: join(HERE, "client-entry.tsx"),
  platform: "browser",
  transform: {
    jsx: jsxTransformOptions(NODE_ENV !== "production"),
    define: {
      "process.env": JSON.stringify({ NODE_ENV, TSS_SERVER_FN_BASE: SERVER_FN_BASE }),
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
      "#tanstack-start-entry": join(HERE, "start-entry.ts"),
      "#tanstack-start-plugin-adapters": join(HERE, "plugin-adapters.ts"),
      "tanstack-start-manifest:v": join(HERE, "manifest.ts"),
      "@tanstack/start-fn-stubs": join(HERE, "fn-stubs.mjs"),
    },
  },
  plugins: [
    closureRecorder,
    makeVitePlugins({ container, appRoot: APP, mode: "dev" }),
    assetsPlugin({ mode: "dev", cssUrls }),
    serverFnClient,
    nodeBuiltinShims,
  ],
  output: {
    format: "esm",
    banner:
      `globalThis.process=globalThis.process||{env:{NODE_ENV:${JSON.stringify(NODE_ENV)},TSS_SERVER_FN_BASE:${JSON.stringify(SERVER_FN_BASE)}}};` +
      "globalThis.global=globalThis.global||globalThis;",
  },
  write: false,
});

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
writeFileSync(
  join(HERE, "client-chunks.json"),
  JSON.stringify({ entry: entryChunk.fileName, files: chunkIndex }),
);
// Module count for the editor's update-progress narration ("(N modules)").
writeFileSync(join(HERE, "client-entry.modules"), String(moduleCount));

const closure = new Set([routerEntry(), ...visitedIds, ...(container?.configDependencies ?? []), ...(container?.watchFiles ?? [])]);
for (const o of result.output) {
  if (o.type !== "chunk") continue;
  for (const id of Object.keys(o.modules ?? {})) closure.add(id);
}
const closureFiles = [...closure]
  .map((id) => String(id).split("?")[0])
  .filter((p) => !p.startsWith("\0"))
  .map((p) => (isAbsolute(p) ? p : resolve(APP, p)))
  .filter((p) => !p.startsWith(HERE) && !/(^|\/)routeTree\.gen\.[jt]sx?$/.test(p))
  .filter((p) => {
    try { return statSync(p).isFile(); } catch { return false; }
  })
  .sort();
writeFileSync(join(HERE, "closure.json"), JSON.stringify(closureFiles));

const devManifest = {
  routes: {
    __root__: {
      preloads: ["/@oj-start/client-entry.js"],
      css: cssUrls,
      scripts: [{ attrs: { type: "module", async: true, src: "/@oj-start/client-entry.js" } }],
    },
  },
};
writeFileSync(
  join(HERE, "manifest.ts"),
  `export const tsrStartManifest = () => (${JSON.stringify(devManifest)});\n`,
);
writeFileSync(join(HERE, "css-urls.json.tmp"), JSON.stringify(cssUrls));
renameSync(join(HERE, "css-urls.json.tmp"), join(HERE, "css-urls.json"));
const _ojTTY = process.stderr.isTTY && !process.env.NO_COLOR;
const OJ = _ojTTY ? "\x1b[48;2;255;255;255m\x1b[1;38;2;42;51;212m oj \x1b[0m" : "oj";
process.stderr.write(`${OJ}${_ojTTY ? "" : ":"} client bundled (${result.output.length} files)\n`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) await main();
