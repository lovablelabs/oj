// SPDX-License-Identifier: MIT

// Test a Start app with nitro/vite under `oj dev`. Pages must render in Nitro's
// worker, not oj's Node runner. Check startup, live reload, hydration, server
// function calls, a Nitro route and updated HTML after an edit.
// Install the fixture's locked deps and Nitro nightly in the app itself:
// Nitro's worker needs a real local node_modules, not a symlink or parent dir.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { assertHydrates, parseBoundPort } from "./lib/hydration.mjs";

// Nitro nightly with dev worker and route fixes (nitrojs/nitro#4633).
const NITRO = "nitro@npm:nitro-nightly@3.0.1-20260915-165946-4f902851";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const REQ_PORT = 6865;
let PORT = REQ_PORT;

const installed =
  fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(fixture, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP start nitro dev: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-nitro-dev-"));
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
  execSync(`npm install --no-audit --no-fund --no-save ${NITRO}`, { cwd: app, stdio: "inherit" });
  const original = fs.readFileSync(path.join(fixture, "vite.config.ts"), "utf8");
  const withImport = original.replace(
    'import { defineConfig } from "vite";',
    'import { defineConfig } from "vite";\nimport { nitro } from "nitro/vite";',
  );
  const config = withImport.replace(/^(\s*)tanstackStart\(/m, '$1nitro({ serverDir: "./server" }),\n$1tanstackStart(');
  if (config === original || config === withImport) throw new Error("fixture vite.config.ts changed shape; update this script");
  fs.writeFileSync(path.join(app, "vite.config.ts"), config);
  // The worker uses the app's Vite pipeline, which lacks plugins for the full
  // fixture's CommonJS dep and tsconfig alias. Use a simpler page that reports
  // the server thread, checks client mounting and calls a server function.
  fs.writeFileSync(path.join(app, "src", "routes", "index.tsx"), [
    'import { createRoute, useLoaderData } from "@tanstack/react-router";',
    'import { useEffect, useState } from "react";',
    'import { rootRoute } from "./__root";',
    'import { getGreeting } from "../server/data";',
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
    '      <p data-testid="server-fn">{data.message} / runtime={data.runtime}</p>',
    '      {mounted ? <span data-testid="client-mounted">client-mounted-ok</span> : null}',
    '      <button data-testid="call-fn" onClick={() => getGreeting().then((r) => setCalled(`${r.message}:${r.runtime}`))}>call: {called}</button>',
    "    </main>",
    "  );",
    "}",
    "",
  ].join("\n"));
  fs.writeFileSync(path.join(app, "src", "server", "data.ts"), [
    'import { createServerFn } from "@tanstack/react-start";',
    'import { isMainThread } from "node:worker_threads";',
    "",
    'export const getGreeting = createServerFn({ method: "GET" }).handler(async () => {',
    '  return { message: "server-fn-marker", runtime: isMainThread ? "main-thread" : "nitro-worker" };',
    "});",
    "",
  ].join("\n"));
  fs.mkdirSync(path.join(app, "server", "routes", "api"), { recursive: true });
  fs.writeFileSync(
    path.join(app, "server", "routes", "api", "nitro-ping.ts"),
    'import { defineHandler } from "nitro";\n\nexport default defineHandler(() => "nitro-ping-ok");\n',
  );
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
  return { status: res.status, body: await res.text() };
};

async function runDev() {
  let log = "";
  const srv = spawn(oj, ["dev", app, "--port", String(PORT)], {
    cwd: app,
    detached: true,
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, CI: "1", NO_COLOR: "1", FORCE_COLOR: "0" },
  });
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  const stop = () => {
    try { process.kill(-srv.pid, "SIGTERM"); } catch {}
    setTimeout(() => { try { process.kill(-srv.pid, "SIGKILL"); } catch {} }, 2000).unref();
  };
  try {
    for (let i = 0; i < 240; i++) {
      if (srv.exitCode != null) break;
      const bound = parseBoundPort(log);
      if (bound) { PORT = bound; break; }
      await new Promise((r) => setTimeout(r, 250));
    }
    let up = false;
    for (let i = 0; i < 240 && !up; i++) {
      if (srv.exitCode != null) break;
      try { up = (await fetch(`http://127.0.0.1:${PORT}/`)).status === 200; } catch {}
      if (!up) await new Promise((r) => setTimeout(r, 500));
    }
    if (!up) throw new Error(`oj dev did not serve on :${PORT}; log:\n${log.slice(-4000)}`);

    const home = await get("/");
    if (home.status !== 200) throw new Error(`/ returned ${home.status}`);
    for (const [what, marker] of [
      ["home render", "HOME!"],
      ["server function", "server-fn-marker"],
      ["render in nitro's worker (not oj's Node runner)", "nitro-worker"],
      ["live-reload client", "/@oj-start/live-reload.js"],
    ]) {
      if (!home.body.includes(marker)) {
        throw new Error(`/: missing ${what} ("${marker}")\n${home.body.slice(0, 1500)}\n--- log tail ---\n${log.slice(-3000)}`);
      }
    }

    const ping = await get("/api/nitro-ping");
    if (ping.status !== 200 || !ping.body.includes("nitro-ping-ok")) {
      throw new Error(`/api/nitro-ping (a nitro server route) returned ${ping.status}: ${ping.body.slice(0, 300)}`);
    }

    const about = await get("/about");
    if (about.status !== 200 || !about.body.includes("about-page-marker")) {
      throw new Error(`/about did not render (${about.status})`);
    }

    // Check that Nitro's worker accepts the client module's server-function ID
    // with the headers a browser sends for a same-origin request.
    const clientModule = await get("/src/server/data.ts");
    const fnId = clientModule.body.match(/createClientRpc\((["'`])([^"'`]+)\1\)/)?.[2];
    if (!fnId) throw new Error(`no createClientRpc id in the client data.ts module:\n${clientModule.body.slice(0, 800)}`);
    const rpc = await fetch(`http://127.0.0.1:${PORT}/_serverFn/${fnId}`, {
      headers: { "x-tsr-serverFn": "true", accept: "application/json", origin: `http://127.0.0.1:${PORT}`, "sec-fetch-site": "same-origin" },
    });
    const rpcBody = await rpc.text();
    if (rpc.status !== 200 || !rpcBody.includes("server-fn-marker") || !rpcBody.includes("nitro-worker")) {
      throw new Error(`/_serverFn/${fnId} (browser server-function call): ${rpc.status} ${rpcBody.slice(0, 500)}\n--- log tail ---\n${log.slice(-3000)}`);
    }

    // Check hydration and a server-function call in the browser before editing /about.
    const chromium = await loadPlaywright();
    if (chromium) {
      const browser = await chromium.launch();
      try {
        await assertHydrates(browser, `http://127.0.0.1:${PORT}/`, {
          ssrMarker: "nitro-worker",
          clientMarker: '[data-testid="client-mounted"]',
          clientMarkerText: "client-mounted-ok",
          interaction: { click: '[data-testid="call-fn"]', expect: { selector: '[data-testid="call-fn"]', text: "server-fn-marker:nitro-worker" } },
          whitelist: [/favicon\.ico/],
        });
        await assertHydrates(browser, `http://127.0.0.1:${PORT}/about`, { ssrMarker: "about-page-marker", whitelist: [/favicon\.ico/] });
        console.log("start-nitro-dev: browser hydration ok (/ mounts + browser server-fn call, /about clean)");
      } catch (e) {
        const tail = `\n--- oj server log (tail) ---\n${log.slice(-3000)}`;
        if (e instanceof Error) {
          e.message += tail;
          throw e;
        }
        throw new Error(String(e) + tail);
      } finally {
        await browser.close();
      }
    } else {
      console.log("start-nitro-dev: playwright not installed locally; skipped the browser hydration pass (CI runs it)");
    }

    const target = path.join(app, "src", "routes", "about.tsx");
    fs.writeFileSync(
      target,
      fs.readFileSync(target, "utf8").replace("about-page-marker", "about-page-edited-marker"),
    );
    const t0 = Date.now();
    let fresh = null;
    for (let i = 0; i < 200; i++) {
      const res = await get("/about");
      if (res.status === 200 && res.body.includes("about-page-edited-marker")) {
        fresh = Date.now() - t0;
        break;
      }
      await new Promise((r) => setTimeout(r, 100));
    }
    if (fresh == null) {
      throw new Error(`/about still stale 20s after the edit; log tail:\n${log.slice(-4000)}`);
    }
    console.log(`start-nitro-dev: worker render + nitro route + server-fn call + live-reload client + fresh document ${fresh}ms after the edit`);
  } finally {
    stop();
  }
}

try {
  makeApp();
  await runDev();
} finally {
  cleanup();
}
