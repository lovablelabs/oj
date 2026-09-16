// SPDX-License-Identifier: MIT

// Test that `oj build` with nitro/vite produces Nitro's .output layout, not
// oj's dist server: client assets in public/, a Node server in server/index.mjs
// and build info in nitro.json. SSR runs as a service in Nitro's server.
// Copy the Start fixture, add Nitro nightly and a server route, then build
// and run it. Check all fixture features, the route and browser hydration.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { assertHydrates } from "./lib/hydration.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = 6743;
// Nitro nightly with dev worker and route fixes (nitrojs/nitro#4633).
const NITRO = "nitro@npm:nitro-nightly@3.0.1-20260915-165946-4f902851";

const installed =
  fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(fixture, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP start nitro build: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-nitro-"));
const app = path.join(tmp, "app");
const keep = !!process.env.OJ_E2E_KEEP;
const cleanup = () => { if (!keep) fs.rmSync(tmp, { recursive: true, force: true }); };

function makeApp() {
  fs.mkdirSync(app);
  for (const f of ["src", "public", "styles", "packages", "tsconfig.json", "package.json", "package-lock.json"]) {
    fs.cpSync(path.join(fixture, f), path.join(app, f), { recursive: true });
  }
  execSync("npm install --no-audit --no-fund", { cwd: app, stdio: "inherit" });
  execSync(`npm install --no-audit --no-fund --no-save ${NITRO}`, { cwd: app, stdio: "inherit" });
  const original = fs.readFileSync(path.join(fixture, "vite.config.ts"), "utf8");
  const withImport = original.replace(
    'import { defineConfig } from "vite";',
    'import { defineConfig } from "vite";\nimport { nitro } from "nitro/vite";',
  );
  const config = withImport.replace(/^(\s*)tanstackStart\(/m, '$1nitro({ serverDir: "./server" }),\n$1tanstackStart(');
  if (config === original || config === withImport) throw new Error("fixture vite.config.ts changed shape; update this script");
  fs.writeFileSync(path.join(app, "vite.config.ts"), config);
  fs.mkdirSync(path.join(app, "server", "routes", "api"), { recursive: true });
  fs.writeFileSync(
    path.join(app, "server", "routes", "api", "nitro-ping.ts"),
    'import { defineHandler } from "nitro";\n\nexport default defineHandler(() => "nitro-ping-ok");\n',
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
  for (const rel of ["server.mjs", "worker.mjs", "server-bundle.mjs"]) {
    if (fs.existsSync(path.join(app, "dist", rel))) throw new Error(`dist/${rel} written: the nitro build must not carry oj's Node server`);
  }
  const info = JSON.parse(fs.readFileSync(path.join(out, "nitro.json"), "utf8"));
  if (info.preset !== "node-server" || info.serverEntry !== "server/index.mjs") {
    throw new Error(`.output/nitro.json: unexpected build info ${JSON.stringify(info)}`);
  }
  console.log(`start-nitro: layout ok (nitro ${info.versions?.nitro}, preset ${info.preset})`);
}

async function loadPlaywright() {
  try {
    return (await import("playwright")).chromium;
  } catch {
    return null;
  }
}

const get = async (route) => {
  const res = await fetch(`http://127.0.0.1:${PORT}${route}`);
  return { status: res.status, type: res.headers.get("content-type") || "", cache: res.headers.get("cache-control") || "", body: await res.text() };
};

async function runServer() {
  let log = "";
  const srv = spawn(process.execPath, [path.join(app, ".output", "server", "index.mjs")], {
    cwd: tmp,
    detached: true,
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, PORT: String(PORT), HOST: "127.0.0.1" },
  });
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  const stop = () => {
    try { process.kill(-srv.pid, "SIGTERM"); } catch {}
    setTimeout(() => { try { process.kill(-srv.pid, "SIGKILL"); } catch {} }, 2000).unref();
  };
  try {
    let up = false;
    for (let i = 0; i < 120 && !up; i++) {
      if (srv.exitCode != null) break;
      try { up = (await fetch(`http://127.0.0.1:${PORT}/api/nitro-ping`)).status === 200; } catch {}
      if (!up) await new Promise((r) => setTimeout(r, 250));
    }
    if (!up) throw new Error(`.output/server/index.mjs did not serve on :${PORT}; log:\n${log.slice(-3000)}`);

    const home = await get("/");
    if (home.status !== 200) throw new Error(`/ returned ${home.status}\n${home.body.slice(0, 1500)}\n${log.slice(-2000)}`);
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
    if (!/<link[^>]*rel="stylesheet"[^>]*\.css/.test(h.slice(0, h.indexOf("</head>")))) throw new Error("stylesheet not linked in the SSR head");

    const about = await get("/about");
    if (about.status !== 200 || !about.body.includes("about-page-marker")) throw new Error(`/about did not render (${about.status})`);

    const ping = await get("/api/nitro-ping");
    if (ping.status !== 200 || ping.body !== "nitro-ping-ok") throw new Error(`/api/nitro-ping (a nitro server route): ${ping.status} ${ping.body.slice(0, 200)}`);

    // Check that Nitro serves hashed client assets with immutable caching.
    const script = h.match(/<script[^>]*src="([^"]+\.js)"/)?.[1] ?? h.match(/href="(\/assets\/[^"]+\.js)"/)?.[1];
    if (!script) throw new Error("no client script in the SSR html");
    const js = await get(script);
    if (js.status !== 200 || !/javascript/.test(js.type)) throw new Error(`client bundle ${script}: ${js.status} ${js.type}`);
    if (!js.cache.includes("immutable")) throw new Error(`client bundle ${script}: cache-control ${JSON.stringify(js.cache)} is not immutable`);
    // Check that SSR accepts a server-function ID from the client bundle with
    // same-origin browser headers. IDs encode `<file>#<export>` as base64url.
    const fnId = fs.readdirSync(path.join(app, ".output", "public", "assets"))
      .filter((f) => f.endsWith(".js"))
      .flatMap((f) => fs.readFileSync(path.join(app, ".output", "public", "assets", f), "utf8").match(/[A-Za-z0-9_-]{16,}/g) ?? [])
      .find((token) => Buffer.from(token, "base64url").toString("utf8").endsWith("data.ts#getGreeting"));
    if (!fnId) throw new Error("no getGreeting server-function id in the client bundle");
    const rpc = await fetch(`http://127.0.0.1:${PORT}/_serverFn/${fnId}`, {
      headers: { "x-tsr-serverFn": "true", accept: "application/json", origin: `http://127.0.0.1:${PORT}`, "sec-fetch-site": "same-origin" },
    });
    const rpcBody = await rpc.text();
    if (rpc.status !== 200 || !rpcBody.includes("server-fn-marker")) {
      throw new Error(`/_serverFn/${fnId} (browser server-function call): ${rpc.status} ${rpcBody.slice(0, 500)}\n${log.slice(-2000)}`);
    }
    const pub = await get("/favicon.txt");
    if (pub.status !== 200 || !pub.body.includes("public-dir-marker")) throw new Error("publicDir file not served as a static asset");

    // Check hydration from .output/public: the page mounts and the counter works.
    const chromium = await loadPlaywright();
    if (chromium) {
      const browser = await chromium.launch();
      try {
        await assertHydrates(browser, `http://127.0.0.1:${PORT}/`, {
          ssrMarker: "HOME!",
          clientMarker: '[data-testid="client-mounted"]',
          clientMarkerText: "client-mounted-ok",
          interaction: { click: '[data-testid="counter"]', expect: { selector: '[data-testid="counter"]', text: "count: 1" } },
          whitelist: [/favicon\.ico/],
        });
        await assertHydrates(browser, `http://127.0.0.1:${PORT}/about`, { ssrMarker: "about-page-marker", whitelist: [/favicon\.ico/] });
        console.log("start-nitro: browser hydration ok (/ mounts + counter interaction, /about clean)");
      } catch (e) {
        const tail = `\n--- server log (tail) ---\n${log.slice(-3000)}`;
        if (e instanceof Error) {
          e.message += tail;
          throw e;
        }
        throw new Error(String(e) + tail);
      } finally {
        await browser.close();
      }
    } else {
      console.log("start-nitro: playwright not installed locally; skipped the browser hydration pass (CI runs it)");
    }

    console.log(`start-nitro: node-server render ok (${want.length} features + /about + nitro route + server-fn call + assets + publicDir)`);
  } finally {
    stop();
  }
}

try {
  makeApp();
  execSync(`${JSON.stringify(oj)} build ${JSON.stringify(app)}`, { cwd: app, stdio: "inherit" });
  assertLayout();
  await runServer();
  cleanup();
} catch (e) {
  console.error(e?.stack || e);
  if (keep) console.error(`kept ${tmp}`);
  else cleanup();
  process.exit(1);
}
