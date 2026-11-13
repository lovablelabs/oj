// SPDX-License-Identifier: MIT

// Test that `oj build` with nitro/vite produces Nitro's .output layout, not
// oj's dist server: client assets in public/, a Node server in server/index.mjs
// and build info in nitro.json. SSR runs as a service in Nitro's server.
// Copy the Start fixture, add Nitro and server routes, then build
// and run it. Check all fixture features, the routes and browser hydration.
// The Nitro server routes use the fixture's plugin virtual module,
// import.meta.glob and a ?raw import: Vite runs the app's plugins and its
// transforms for the nitro environment too, so oj's Nitro build must.
// Then rebuild with the Vercel preset's immutable static files, whose
// buildAssetsDir the client must be written under, and check that a broken
// Nitro route fails the build.
// Last, `oj dev` on the same app: pages render in Nitro's worker, not oj's
// Node runner; a Nitro route, server functions, hydration and an edit's
// fresh document work. Nitro's worker needs a real local node_modules, so the
// app installs its deps itself.

import { spawn, spawnSync, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { NITRO_DEPS, settles, waitUp } from "./util.mjs";
import { assertHydrates, parseBoundPort } from "./lib/hydration.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = 6743;
const DEV_PORT = 6865;

const installed = fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start"));
if (!installed) {
  console.log("SKIP start nitro build: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-nitro-"));
const app = path.join(tmp, "app");
const keep = !!process.env.OJ_E2E_KEEP;
const cleanup = () => {
  if (keep) return;
  for (let i = 0; ; i++) {
    try {
      return fs.rmSync(tmp, { recursive: true, force: true });
    } catch (e) {
      if (i >= 20) throw e;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100);
    }
  }
};

function makeApp() {
  fs.mkdirSync(app);
  for (const f of ["src", "public", "styles", "packages", "tsconfig.json", "package.json", "package-lock.json"]) {
    fs.cpSync(path.join(fixture, f), path.join(app, f), { recursive: true });
  }
  execSync("npm install --no-audit --no-fund", { cwd: app, stdio: "inherit" });
  execSync(`npm install --no-audit --no-fund --no-save ${NITRO_DEPS}`, { cwd: app, stdio: "inherit" });
  const original = fs.readFileSync(path.join(fixture, "vite.config.ts"), "utf8");
  const withImport = original.replace(
    'import { defineConfig } from "vite";',
    'import { defineConfig } from "vite";\nimport { nitro } from "nitro/vite";',
  );
  const config = withImport.replace(/^(\s*)tanstackStart\(/m, '$1nitro({ serverDir: "./server" }),\n$1tanstackStart(');
  if (config === original || config === withImport)
    throw new Error("fixture vite.config.ts changed shape; update this script");
  fs.writeFileSync(path.join(app, "vite.config.ts"), config);
  fs.mkdirSync(path.join(app, "server", "routes", "api"), { recursive: true });
  fs.writeFileSync(
    path.join(app, "server", "routes", "api", "nitro-ping.ts"),
    'import { defineHandler } from "nitro";\n\nexport default defineHandler(() => "nitro-ping-ok");\n',
  );
  fs.writeFileSync(
    path.join(app, "server", "routes", "api", "nitro-virtual.ts"),
    'import { defineHandler } from "nitro";\nimport { buildTag } from "virtual:build-info";\n\nexport default defineHandler(() => `virtual:${buildTag}`);\n',
  );
  fs.writeFileSync(
    path.join(app, "server", "routes", "api", "nitro-glob.ts"),
    [
      'import { defineHandler } from "nitro";',
      'const mods = import.meta.glob("../../../src/content/*.json", { eager: true }) as Record<string, { default: { title: string } }>;',
      "",
      'export default defineHandler(() => "glob:" + Object.values(mods).map((m) => m.default.title).sort().join(", "));',
      "",
    ].join("\n"),
  );
  fs.writeFileSync(
    path.join(app, "server", "routes", "api", "nitro-raw.ts"),
    'import { defineHandler } from "nitro";\nimport notes from "../../../src/content/notes.txt?raw";\n\nexport default defineHandler(() => "raw:" + notes);\n',
  );
}

function assertLayout() {
  const out = path.join(app, ".output");
  for (const rel of ["nitro.json", "server/index.mjs", "public/favicon.txt"]) {
    if (!fs.existsSync(path.join(out, rel))) throw new Error(`missing .output/${rel}`);
  }
  if (!fs.readdirSync(path.join(out, "public", "assets")).some((f) => f.endsWith(".js"))) {
    throw new Error("no client bundle in .output/public/assets");
  }
  if (fs.existsSync(path.join(app, "dist")))
    throw new Error("dist/ written: nitro owns the output (its output.dir), oj's own server is not built");
  const info = JSON.parse(fs.readFileSync(path.join(out, "nitro.json"), "utf8"));
  if (info.preset !== "node-server" || info.serverEntry !== "server/index.mjs") {
    throw new Error(`.output/nitro.json: unexpected build info ${JSON.stringify(info)}`);
  }
  console.log(`start-nitro: layout ok (nitro ${info.versions?.nitro}, preset ${info.preset})`);
}

// Nitro's Vercel preset with immutable static files moves the client's
// assetsDir to its buildAssetsDir (`_vercel/immutable/<framework>`): oj writes
// the client there, Nitro lists it in immutable.json and caches it.
function buildVercelImmutable() {
  const env = { ...process.env, NITRO_PRESET: "vercel", NITRO_VERCEL_IMMUTABLE_STATIC_FILES_ENABLED: "1" };
  // Off Vercel's own build: no Vercel deployment gating.
  for (const k of Object.keys(env)) if (k.startsWith("VERCEL")) delete env[k];
  const log = execSync(`${JSON.stringify(oj)} build ${JSON.stringify(app)} 2>&1`, { cwd: app, env, encoding: "utf8" });
  if (/no assets were emitted under/.test(log))
    throw new Error(`nitro found no client assets in its buildAssetsDir:\n${log}`);
  const out = path.join(app, ".vercel", "output");
  const immutable = path.join(out, "static", "_vercel", "immutable");
  const files = fs.existsSync(immutable) ? fs.readdirSync(immutable, { recursive: true }).map(String) : [];
  if (!files.some((f) => f.endsWith(".js")) || !files.some((f) => f.endsWith(".css")))
    throw new Error(`no client bundle or stylesheet under static/_vercel/immutable: ${JSON.stringify(files)}`);
  if (fs.existsSync(path.join(out, "static", "assets")))
    throw new Error("client assets written to static/assets, not Nitro's buildAssetsDir");
  const manifest = JSON.parse(fs.readFileSync(path.join(out, "immutable.json"), "utf8"));
  const urls = Object.keys(manifest.hashes ?? {});
  if (!urls.length || !urls.every((u) => u.startsWith("/_vercel/immutable/")))
    throw new Error(`immutable.json lists ${JSON.stringify(urls)}`);
  const config = JSON.parse(fs.readFileSync(path.join(out, "config.json"), "utf8"));
  const rule = config.routes.find((r) => r.src?.startsWith("^\\/_vercel\\/immutable"));
  if (!rule?.headers?.["cache-control"]?.includes("immutable"))
    throw new Error(`no immutable cache rule for the buildAssetsDir in config.json: ${JSON.stringify(config.routes)}`);
  // The server's manifest names the client where it was written.
  const server = path.join(out, "functions", "__server.func");
  const entry = urls.find((u) => /\/client-[^/]+\.js$/.test(u));
  const named = fs
    .readdirSync(server, { recursive: true })
    .filter((f) => String(f).endsWith(".mjs"))
    .some((f) => fs.readFileSync(path.join(server, String(f)), "utf8").includes(entry));
  if (!entry || !named) throw new Error(`the server bundle does not name the client entry ${entry}`);
  console.log(`start-nitro: vercel immutable static files ok (${urls.length} files)`);
}

async function loadPlaywright() {
  try {
    return (await import("playwright")).chromium;
  } catch {
    return null;
  }
}

const get = async (route, port = PORT) => {
  const res = await fetch(`http://127.0.0.1:${port}${route}`);
  return {
    status: res.status,
    type: res.headers.get("content-type") || "",
    cache: res.headers.get("cache-control") || "",
    body: await res.text(),
  };
};

// A server process in its own group, with its output captured.
function serve(cmd, args, options) {
  const srv = spawn(cmd, args, { ...options, detached: true, stdio: ["ignore", "pipe", "pipe"] });
  let log = "";
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  const stop = () => {
    try {
      process.kill(-srv.pid, "SIGTERM");
    } catch {}
    setTimeout(() => {
      try {
        process.kill(-srv.pid, "SIGKILL");
      } catch {}
    }, 2000).unref();
  };
  return { srv, log: () => log, stop };
}

// A server-function call with the headers a browser sends for a same-origin
// request.
async function serverFnCall(port, fnId) {
  const res = await fetch(`http://127.0.0.1:${port}/_serverFn/${fnId}`, {
    headers: {
      "x-tsr-serverFn": "true",
      accept: "application/json",
      origin: `http://127.0.0.1:${port}`,
      "sec-fetch-site": "same-origin",
    },
  });
  return { status: res.status, body: await res.text() };
}

// Hydration failures carry the server's log tail.
async function withLogTail(log, fn) {
  try {
    return await fn();
  } catch (e) {
    const tail = `\n--- server log (tail) ---\n${log().slice(-3000)}`;
    if (e instanceof Error) {
      e.message += tail;
      throw e;
    }
    throw new Error(String(e) + tail);
  }
}

async function runServer() {
  const { srv, log, stop } = serve(process.execPath, [path.join(app, ".output", "server", "index.mjs")], {
    cwd: tmp,
    env: { ...process.env, PORT: String(PORT), HOST: "127.0.0.1" },
  });
  try {
    await waitUp(`http://127.0.0.1:${PORT}/api/nitro-ping`, { proc: srv, timeoutMs: 30000 }).catch((e) => {
      throw new Error(`.output/server/index.mjs did not serve on :${PORT} (${e.message}); log:\n${log().slice(-3000)}`);
    });

    const home = await get("/");
    if (home.status !== 200)
      throw new Error(`/ returned ${home.status}\n${home.body.slice(0, 1500)}\n${log().slice(-2000)}`);
    const h = home.body;
    const want = [
      ["#lib alias (shout)", "HOME!"],
      ["server function", "server-fn-marker"],
      ["tsconfig paths alias", "ALIAS!"],
      ["commonjs dep facade", "[INTEROP]"],
      ["commonjs subpath (extensionless)", "[deep:ok]"],
      ["plugin virtual module", "fixture-virtual-ok"],
      ["plugin load override + buildStart + this.environment.config.consumer", "FRESH_via_buildStart_ssr-server"],
      ["import.meta.glob", "Alpha Widget, Beta Widget"],
      ["?raw import", "raw-notes-marker"],
      ["svgr bare .svg component", "<rect"],
      ["svgr ?react component", "<polygon"],
      ["mdx module", "mdx-content-marker"],
      ["config define applied", "fixture-define-marker"],
      ["import.meta.env in a plain .js module", "jsenv:production:true"],
    ];
    for (const [what, marker] of want) {
      if (!h.includes(marker)) throw new Error(`nitro render: missing ${what} ("${marker}")\n${h.slice(0, 1500)}`);
    }
    if (!/<link[^>]*rel="stylesheet"[^>]*\.css/.test(h.slice(0, h.indexOf("</head>"))))
      throw new Error("stylesheet not linked in the SSR head");

    const about = await get("/about");
    if (about.status !== 200 || !about.body.includes("about-page-marker"))
      throw new Error(`/about did not render (${about.status})`);

    const ping = await get("/api/nitro-ping");
    if (ping.status !== 200 || ping.body !== "nitro-ping-ok")
      throw new Error(`/api/nitro-ping (a nitro server route): ${ping.status} ${ping.body.slice(0, 200)}`);
    // Nitro server routes built with the app's plugins and Vite's transforms.
    for (const [route, marker, what] of [
      ["/api/nitro-virtual", "virtual:fixture-virtual-ok", "plugin virtual module"],
      ["/api/nitro-glob", "glob:Alpha Widget, Beta Widget", "import.meta.glob"],
      ["/api/nitro-raw", "raw:raw-notes-marker", "?raw import"],
    ]) {
      const res = await get(route);
      if (res.status !== 200 || !res.body.startsWith(marker)) {
        throw new Error(
          `${route} (a nitro server route using ${what}): ${res.status} ${res.body.slice(0, 300)}\n${log().slice(-2000)}`,
        );
      }
    }

    // Check that Nitro serves hashed client assets with immutable caching.
    const script = h.match(/<script[^>]*src="([^"]+\.js)"/)?.[1] ?? h.match(/href="(\/assets\/[^"]+\.js)"/)?.[1];
    if (!script) throw new Error("no client script in the SSR html");
    const js = await get(script);
    if (js.status !== 200 || !/javascript/.test(js.type))
      throw new Error(`client bundle ${script}: ${js.status} ${js.type}`);
    if (!js.cache.includes("immutable"))
      throw new Error(`client bundle ${script}: cache-control ${JSON.stringify(js.cache)} is not immutable`);
    const pub = await get("/favicon.txt");
    if (pub.status !== 200 || !pub.body.includes("public-dir-marker"))
      throw new Error("publicDir file not served as a static asset");

    // Check hydration from .output/public: the page mounts and the counter works.
    const chromium = await loadPlaywright();
    if (chromium) {
      const browser = await chromium.launch();
      try {
        await withLogTail(log, () =>
          assertHydrates(browser, `http://127.0.0.1:${PORT}/`, {
            ssrMarker: "HOME!",
            clientMarker: '[data-testid="client-mounted"]',
            clientMarkerText: "client-mounted-ok",
            interaction: {
              click: '[data-testid="counter"]',
              expect: { selector: '[data-testid="counter"]', text: "count: 1" },
            },
            whitelist: [/favicon\.ico/],
          }),
        );
        console.log("start-nitro: browser hydration ok (/ mounts + counter interaction)");
      } finally {
        await browser.close();
      }
    } else {
      console.log("start-nitro: playwright not installed locally; skipped the browser hydration pass (CI runs it)");
    }

    console.log(
      `start-nitro: node-server render ok (${want.length} features + /about + nitro routes (plain, virtual, glob, raw) + assets + publicDir)`,
    );
  } finally {
    stop();
  }
}

// A broken Nitro route fails the nitro environment's build, which fails
// `oj build` (exit status and output) whether or not Nitro's hook rethrows it.
function assertBrokenRouteFails() {
  const broken = path.join(app, "server", "routes", "api", "broken.ts");
  fs.writeFileSync(broken, "export default (;\n");
  try {
    const r = spawnSync(oj, ["build", app], { cwd: app, encoding: "utf8" });
    const output = `${r.stdout}${r.stderr}`;
    if (r.status === 0 || !output.includes("the nitro environment"))
      throw new Error(`broken nitro route: exit ${r.status}\n${output.slice(-2000)}`);
  } finally {
    fs.rmSync(broken);
  }
  console.log("start-nitro: a broken nitro route fails the build");
}

// The dev worker runs the app's Vite pipeline, which lacks plugins for the
// full fixture's CommonJS dep: a simpler home page reports the server thread,
// uses the tsconfig alias, checks client mounting and calls a server function.
function useDevPages() {
  fs.writeFileSync(
    path.join(app, "src", "routes", "index.tsx"),
    [
      'import { createRoute, useLoaderData } from "@tanstack/react-router";',
      'import { useEffect, useState } from "react";',
      'import { rootRoute } from "./__root";',
      'import { getGreeting } from "../server/data";',
      'import { shout } from "@app/lib/format";',
      "",
      "export const indexRoute = createRoute({",
      "  getParentRoute: () => rootRoute,",
      '  path: "/",',
      "  loader: async () => await getGreeting(),",
      "  component: Index,",
      "});",
      "",
      "function Index() {",
      "  const data = useLoaderData({ from: indexRoute.id });",
      "  const [mounted, setMounted] = useState(false);",
      '  const [called, setCalled] = useState("");',
      "  useEffect(() => setMounted(true), []);",
      "  return (",
      "    <main>",
      '      <h1 className="fixture-heading">HOME!</h1>',
      '      <p>{shout("alias-dev")}</p>',
      '      <p data-testid="server-fn">{data.message} / runtime={data.runtime}</p>',
      '      {mounted ? <span data-testid="client-mounted">client-mounted-ok</span> : null}',
      '      <button data-testid="call-fn" onClick={() => getGreeting().then((r) => setCalled(`${r.message}:${r.runtime}`))}>call: {called}</button>',
      "    </main>",
      "  );",
      "}",
      "",
    ].join("\n"),
  );
  fs.writeFileSync(
    path.join(app, "src", "server", "data.ts"),
    [
      'import { createServerFn } from "@tanstack/react-start";',
      'import { isMainThread } from "node:worker_threads";',
      "",
      'export const getGreeting = createServerFn({ method: "GET" }).handler(async () => {',
      '  return { message: "server-fn-marker", runtime: isMainThread ? "main-thread" : "nitro-worker" };',
      "});",
      "",
    ].join("\n"),
  );
}

async function runDev() {
  const { srv, log, stop } = serve(oj, ["dev", app, "--port", String(DEV_PORT)], {
    cwd: app,
    env: { ...process.env, CI: "1", NO_COLOR: "1", FORCE_COLOR: "0" },
  });
  try {
    // oj dev moves to a free port when the asked one is taken.
    let port = DEV_PORT;
    await settles(
      () => {
        const bound = parseBoundPort(log());
        if (bound) port = bound;
        return !!bound || srv.exitCode != null;
      },
      { timeoutMs: 60000 },
    );
    await waitUp(`http://127.0.0.1:${port}/`, { proc: srv }).catch((e) => {
      throw new Error(`oj dev did not serve on :${port} (${e.message}); log:\n${log().slice(-4000)}`);
    });

    const home = await get("/", port);
    if (home.status !== 200) throw new Error(`/ returned ${home.status}`);
    for (const [what, marker] of [
      ["home render", "HOME!"],
      ["tsconfig paths alias", "ALIAS-DEV!"],
      ["server function", "server-fn-marker"],
      ["render in nitro's worker (not oj's Node runner)", "nitro-worker"],
      ["live-reload client", "/@oj-start/live-reload.js"],
    ]) {
      if (!home.body.includes(marker)) {
        throw new Error(
          `/: missing ${what} ("${marker}")\n${home.body.slice(0, 1500)}\n--- log tail ---\n${log().slice(-3000)}`,
        );
      }
    }

    const ping = await get("/api/nitro-ping", port);
    if (ping.status !== 200 || !ping.body.includes("nitro-ping-ok")) {
      throw new Error(`/api/nitro-ping (a nitro server route) returned ${ping.status}: ${ping.body.slice(0, 300)}`);
    }
    const about = await get("/about", port);
    if (about.status !== 200 || !about.body.includes("about-page-marker")) {
      throw new Error(`/about did not render (${about.status})`);
    }

    // Nitro's worker accepts the client module's server-function ID.
    const clientModule = await get("/src/server/data.ts", port);
    const fnId = clientModule.body.match(/createClientRpc\((["'`])([^"'`]+)\1\)/)?.[2];
    if (!fnId)
      throw new Error(`no createClientRpc id in the client data.ts module:\n${clientModule.body.slice(0, 800)}`);
    const rpc = await serverFnCall(port, fnId);
    if (rpc.status !== 200 || !rpc.body.includes("server-fn-marker") || !rpc.body.includes("nitro-worker")) {
      throw new Error(
        `/_serverFn/${fnId} (browser server-function call): ${rpc.status} ${rpc.body.slice(0, 500)}\n--- log tail ---\n${log().slice(-3000)}`,
      );
    }

    // Hydration and a server-function call in the browser, before editing /about.
    const chromium = await loadPlaywright();
    if (chromium) {
      const browser = await chromium.launch();
      try {
        await withLogTail(log, () =>
          assertHydrates(browser, `http://127.0.0.1:${port}/`, {
            ssrMarker: "nitro-worker",
            clientMarker: '[data-testid="client-mounted"]',
            clientMarkerText: "client-mounted-ok",
            interaction: {
              click: '[data-testid="call-fn"]',
              expect: { selector: '[data-testid="call-fn"]', text: "server-fn-marker:nitro-worker" },
            },
            whitelist: [/favicon\.ico/],
          }),
        );
        console.log("start-nitro: dev browser hydration ok (/ mounts + browser server-fn call)");
      } finally {
        await browser.close();
      }
    } else {
      console.log("start-nitro: playwright not installed locally; skipped the dev browser hydration pass (CI runs it)");
    }

    const target = path.join(app, "src", "routes", "about.tsx");
    fs.writeFileSync(target, fs.readFileSync(target, "utf8").replace("about-page-marker", "about-page-edited-marker"));
    const t0 = Date.now();
    const fresh = await settles(
      async () => {
        const res = await get("/about", port);
        return res.status === 200 && res.body.includes("about-page-edited-marker");
      },
      { timeoutMs: 20000 },
    );
    if (!fresh) throw new Error(`/about still stale 20s after the edit; log tail:\n${log().slice(-4000)}`);
    console.log(
      `start-nitro: dev worker render + nitro route + server-fn call + live-reload client + fresh document ${Date.now() - t0}ms after the edit`,
    );
  } finally {
    stop();
  }
}

try {
  makeApp();
  execSync(`${JSON.stringify(oj)} build ${JSON.stringify(app)}`, { cwd: app, stdio: "inherit" });
  assertLayout();
  await runServer();
  buildVercelImmutable();
  assertBrokenRouteFails();
  useDevPages();
  await runDev();
  cleanup();
} catch (e) {
  console.error(e?.stack || e);
  if (keep) console.error(`kept ${tmp}`);
  else cleanup();
  process.exit(1);
}
