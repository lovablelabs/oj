// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// A TanStack Start app on Cloudflare under `oj dev`: a route renders on the
// worker (Miniflare/workerd) and, during SSR, fetches a same-origin API path
// that `server.proxy` is configured to forward to an upstream. The plugin
// routes the worker's OUTBOUND fetch back into the dev server's middleware
// stack, so the proxy must live there (as it does in Vite) or the request
// never reaches the upstream: it loops back into the worker and the SSR stream
// never closes (the hydration wedge).
//
// This fixture proves two defects and their fix:
//   1. WORKER PATH: `GET /wedge` renders in the worker; its loader fetches
//      `/go-api/data`. Without a proxy in the middleware stack the fetch is
//      dispatched back into the worker (the Cloudflare catch-all), where a
//      deliberately-hanging `/go-api/*` route never answers -> `/wedge` wedges.
//      With the proxy it is forwarded to the upstream (stripped to `/data`) and
//      `/wedge` returns hydrated HTML fast.
//   2. BROWSER PATH (defect A): a direct `GET /go-api/data` must proxy AND
//      apply the FUNCTION rewrite (strip `/go-api`). The upstream only answers
//      the stripped `/data` and hangs on anything else, so an unstripped
//      forward wedges too.
//
// Deps come from the same recipe as start-cloudflare-dev.mjs (OJ_E2E_CF_DEPS or
// an npm install), and the base node_modules is the symlink in this worktree.

import { spawn, execSync } from "node:child_process";
import http from "node:http";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { settles } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = 6890; // oj dev server
const UPSTREAM = 6891; // proxy upstream (only answers the STRIPPED /data)

const installed =
  fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(fixture, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP proxy worker fetch: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-proxy-worker-"));
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

function installCloudflareDeps() {
  const prepared = process.env.OJ_E2E_CF_DEPS;
  if (prepared) {
    fs.symlinkSync(path.join(path.resolve(prepared), "node_modules"), path.join(tmp, "node_modules"), "dir");
    return;
  }
  fs.writeFileSync(path.join(tmp, "package.json"), JSON.stringify({ name: "oj-cf-deps", private: true, type: "module" }));
  execSync("npm install --no-audit --no-fund --no-package-lock @cloudflare/vite-plugin wrangler", { cwd: tmp, stdio: "inherit" });
}

function makeApp() {
  fs.mkdirSync(path.join(app, "src", "routes"), { recursive: true });
  fs.mkdirSync(path.join(app, "src", "server"), { recursive: true });
  for (const f of ["tsconfig.json", "package.json"]) {
    fs.cpSync(path.join(fixture, f), path.join(app, f), { recursive: true });
  }
  fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");
  fs.cpSync(path.join(fixture, "src", "ssr-entry.ts"), path.join(app, "src", "ssr-entry.ts"));
  // A tiny src/lib so package.json's `#lib/*` import map still resolves.
  fs.mkdirSync(path.join(app, "src", "lib"), { recursive: true });
  fs.writeFileSync(path.join(app, "src", "lib", "format.ts"), 'export const shout = (s: string) => s.toUpperCase() + "!";\n');

  fs.writeFileSync(path.join(app, "wrangler.jsonc"), [
    "{",
    '  "name": "oj-proxy-fixture",',
    '  "main": "@tanstack/react-start/server-entry",',
    '  "compatibility_date": "2025-09-01",',
    '  "compatibility_flags": ["nodejs_compat"],',
    '  "vars": { "EDITION": "fixture-edition" }',
    "}",
    "",
  ].join("\n"));

  // ONE proxy, the app's real config: a FUNCTION rewrite that strips `/go-api`.
  // The upstream only answers the stripped `/data`, so the function must be
  // applied (Vite honours it; oj's old Rust-only proxy dropped it).
  fs.writeFileSync(path.join(app, "vite.config.ts"), [
    'import { tanstackStart } from "@tanstack/react-start/plugin/vite";',
    'import react from "@vitejs/plugin-react";',
    'import { defineConfig } from "vite";',
    'import { cloudflare } from "@cloudflare/vite-plugin";',
    "",
    "export default defineConfig({",
    "  server: {",
    "    proxy: {",
    '      "/go-api": {',
    `        target: "http://127.0.0.1:${UPSTREAM}",`,
    "        changeOrigin: true,",
    "        ws: false,",
    '        rewrite: (p) => p.replace(/^\\/go-api/, ""),',
    "      },",
    "    },",
    "  },",
    "  plugins: [",
    '    cloudflare({ viteEnvironment: { name: "ssr" } }),',
    '    tanstackStart({ server: { entry: "ssr-entry" } }),',
    "    react(),",
    "  ],",
    "});",
    "",
  ].join("\n"));

  fs.writeFileSync(path.join(app, "src", "routes", "__root.tsx"), [
    'import { createRootRoute, HeadContent, Outlet, Scripts } from "@tanstack/react-router";',
    "",
    "export const rootRoute = createRootRoute({",
    '  head: () => ({ meta: [{ title: "oj proxy fixture" }] }),',
    "  component: RootComponent,",
    "});",
    "",
    "function RootComponent() {",
    "  return (",
    '    <html lang="en">',
    "      <head>",
    "        <HeadContent />",
    "      </head>",
    "      <body>",
    "        <Outlet />",
    "        <Scripts />",
    "      </body>",
    "    </html>",
    "  );",
    "}",
    "",
  ].join("\n"));

  fs.writeFileSync(path.join(app, "src", "server", "data.ts"), [
    'import { createServerFn } from "@tanstack/react-start";',
    'import { env } from "cloudflare:workers";',
    "",
    'export const getGreeting = createServerFn({ method: "GET" }).handler(async () => {',
    "  const edition = (env as unknown as Record<string, unknown>).EDITION ?? \"unknown\";",
    '  return { message: "server-fn-marker", edition: String(edition) };',
    "});",
    "",
  ].join("\n"));

  // The control route: renders in the worker (the wrangler var proves it) and
  // completes normally, so a wedged `/wedge` is visibly a `/wedge`-only stall.
  fs.writeFileSync(path.join(app, "src", "routes", "index.tsx"), [
    'import { createRoute, useLoaderData } from "@tanstack/react-router";',
    'import { rootRoute } from "./__root";',
    'import { getGreeting } from "../server/data";',
    'import { shout } from "#lib/format";',
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
    "  return (",
    "    <main>",
    '      <h1>{shout("home")}</h1>',
    '      <p data-testid="server-fn">{data.message} / edition={data.edition}</p>',
    "    </main>",
    "  );",
    "}",
    "",
  ].join("\n"));

  // The wedge route: a page route (so it hydrates) whose loader calls a server
  // function that, inside the worker, fetches the same-origin `/go-api/data`.
  // With the proxy that reaches the upstream (stripped to `/data`); without it
  // the fetch loops back into the worker and hits the hanging `/go-api` route.
  fs.writeFileSync(path.join(app, "src", "routes", "wedge.tsx"), [
    'import { createRoute, useLoaderData } from "@tanstack/react-router";',
    'import { createServerFn } from "@tanstack/react-start";',
    'import { getRequestUrl } from "@tanstack/react-start/server";',
    'import { rootRoute } from "./__root";',
    "",
    'const fetchViaProxy = createServerFn({ method: "GET" }).handler(async () => {',
    "  const base = getRequestUrl();",
    '  const res = await fetch(new URL("/go-api/data", base));',
    "  const json = (await res.json()) as { ok?: boolean; path?: string };",
    "  return { ok: json.ok === true, path: String(json.path ?? '') };",
    "});",
    "",
    "export const wedgeRoute = createRoute({",
    "  getParentRoute: () => rootRoute,",
    '  path: "/wedge",',
    "  loader: async () => await fetchViaProxy(),",
    "  component: Wedge,",
    "});",
    "",
    "function Wedge() {",
    "  const data = useLoaderData({ from: wedgeRoute.id });",
    "  return (",
    "    <main>",
    '      <div id="wedge-result">ok:{String(data.ok)} path:{data.path}</div>',
    "    </main>",
    "  );",
    "}",
    "",
  ].join("\n"));

  // The hanging `/go-api/*` route: reached ONLY when the proxy is missing and
  // the worker's outbound fetch is misrouted back into the worker. It never
  // answers, standing in for the real bug's never-closing SSR stream. With the
  // proxy in the middleware stack this route is never reached.
  fs.writeFileSync(path.join(app, "src", "routes", "go-api.tsx"), [
    'import { createRoute } from "@tanstack/react-router";',
    'import { rootRoute } from "./__root";',
    "",
    "export const goApiRoute = createRoute({",
    "  getParentRoute: () => rootRoute,",
    '  path: "/go-api/$",',
    "  server: {",
    "    handlers: {",
    "      // Never resolves: the misrouted request stalls here (the wedge).",
    "      GET: () => new Promise<Response>(() => {}),",
    "    },",
    "  },",
    "});",
    "",
  ].join("\n"));

  fs.writeFileSync(path.join(app, "src", "routeTree.ts"), [
    'import { rootRoute } from "./routes/__root";',
    'import { indexRoute } from "./routes/index";',
    'import { wedgeRoute } from "./routes/wedge";',
    'import { goApiRoute } from "./routes/go-api";',
    "",
    "export const routeTree = rootRoute.addChildren([indexRoute, wedgeRoute, goApiRoute]);",
    "",
  ].join("\n"));

  fs.writeFileSync(path.join(app, "src", "router.tsx"), [
    'import { createRouter } from "@tanstack/react-router";',
    'import { routeTree } from "./routeTree";',
    "",
    "export function getRouter() {",
    "  return createRouter({ routeTree, defaultPreload: \"intent\", scrollRestoration: true });",
    "}",
    "",
  ].join("\n"));
}

// The upstream: answers ONLY the stripped `/data` (proving the rewrite ran) and
// HANGS on anything else (an unstripped `/go-api/...` never returns). Records
// every path it is asked for.
const upstreamPaths = [];
const upstream = http.createServer((req, res) => {
  upstreamPaths.push(req.url);
  if (req.url === "/data") {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ ok: true, path: req.url }));
    return;
  }
  // Hang: never respond (socket kept open) for any non-stripped path.
});

async function fetchText(url, ms) {
  const ac = new AbortController();
  const t = setTimeout(() => ac.abort(), ms);
  try {
    const res = await fetch(url, { signal: ac.signal });
    return { status: res.status, body: await res.text() };
  } finally {
    clearTimeout(t);
  }
}

async function run() {
  let log = "";
  const srv = spawn(oj, ["dev", app, "--port", String(PORT)], {
    cwd: app,
    detached: true,
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, WRANGLER_SEND_METRICS: "false", CI: "1", NO_COLOR: "1", FORCE_COLOR: "0" },
  });
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  const stop = () => {
    try { process.kill(-srv.pid, "SIGTERM"); } catch {}
    setTimeout(() => { try { process.kill(-srv.pid, "SIGKILL"); } catch {} }, 2000).unref();
  };
  try {
    let up = false;
    await settles(async () => {
      if (srv.exitCode != null) return true;
      try { up = (await fetch(`http://127.0.0.1:${PORT}/`)).status === 200; } catch {}
      return up;
    }, { timeoutMs: 120000, pollMs: 500 });
    if (!up) throw new Error(`oj dev did not serve on :${PORT}; log:\n${log.slice(-4000)}`);

    // Control: the worker renders `/` (the wrangler var proves the worker path
    // is active, so the wedge really is a worker-originated fetch).
    const home = await fetchText(`http://127.0.0.1:${PORT}/`, 15000);
    assert.equal(home.status, 200, `/ returned ${home.status}`);
    assert.match(home.body, /fixture-edition/, `/ did not render on the worker (no wrangler var):\n${home.body.slice(0, 800)}`);

    // DEFECT 1 (worker path): `/wedge` must resolve fast with the proxied,
    // stripped result. Before the fix it wedges (the fetch loops into the
    // hanging worker route) and this times out.
    let wedge = { status: 0, body: "" };
    try {
      wedge = await fetchText(`http://127.0.0.1:${PORT}/wedge`, 15000);
    } catch (e) {
      throw new Error(`/wedge wedged (worker-originated /go-api fetch not proxied): ${e}\nlog tail:\n${log.slice(-3000)}`);
    }
    assert.equal(wedge.status, 200, `/wedge returned ${wedge.status}`);
    // React separates interpolated text with <!-- --> comment nodes in SSR
    // output; strip them before matching the rendered text.
    const wedgeText = wedge.body.replace(/<!--[\s\S]*?-->/g, "");
    assert.match(
      wedgeText,
      /ok:true path:\/data/,
      `/wedge did not carry the proxied, stripped result:\n${wedge.body.slice(0, 800)}`,
    );

    // DEFECT A (browser path) + rewrite strip: a DIRECT `/go-api/data` must
    // proxy and apply the FUNCTION rewrite. The upstream only answers `/data`.
    let direct = { status: 0, body: "" };
    try {
      direct = await fetchText(`http://127.0.0.1:${PORT}/go-api/data`, 15000);
    } catch (e) {
      throw new Error(`browser-direct /go-api/data wedged (unstripped forward reached the upstream): ${e}`);
    }
    assert.equal(direct.status, 200, `/go-api/data returned ${direct.status}`);
    assert.match(direct.body, /"ok":true/, `/go-api/data did not proxy to the upstream:\n${direct.body.slice(0, 400)}`);
    assert.match(direct.body, /"path":"\/data"/, `/go-api/data was not stripped to /data:\n${direct.body.slice(0, 400)}`);

    // The rewrite FUNCTION ran: the upstream only ever saw the stripped path,
    // never an unstripped `/go-api/...`.
    assert.ok(upstreamPaths.includes("/data"), `upstream never received the stripped /data; saw ${JSON.stringify(upstreamPaths)}`);
    assert.ok(
      !upstreamPaths.some((p) => p.startsWith("/go-api")),
      `upstream received an UNSTRIPPED path (rewrite function dropped): ${JSON.stringify(upstreamPaths)}`,
    );

    // The class of check the canary log-greps missed: drive `/wedge` in a real
    // browser and require the document to REACH INTERACTIVE. A wedge hangs the
    // navigation response (the loader awaits a fetch that never resolves), so
    // `waitUntil: "networkidle"` would never fire and this times out — exactly
    // the failure a log-grep for "ok:true" cannot see. When the proxy works the
    // navigation completes, the document parses to `readyState === "complete"`,
    // and the proxied result is in the DOM. Best-effort: skip if playwright is
    // not installed (CI installs it under e2e/).
    let chromium = null;
    try {
      ({ chromium } = createRequire(path.join(here, "x.js"))("playwright"));
    } catch {
      console.log("proxy-worker-fetch: playwright unavailable, skipping the browser interactivity check");
    }
    if (chromium) {
      const browser = await chromium.launch();
      try {
        const page = await browser.newPage();
        // Navigation resolving at all proves the SSR stream closed (a wedged
        // loader would hang it); networkidle proves nothing is left pending.
        await page.goto(`http://127.0.0.1:${PORT}/wedge`, { waitUntil: "networkidle", timeout: 30000 });
        const ready = await page.evaluate(() => document.readyState);
        assert.equal(ready, "complete", `/wedge did not reach interactive (readyState=${ready})`);
        await page.locator("#wedge-result", { hasText: "ok:true path:/data" }).waitFor({ timeout: 20000 });
      } finally {
        await browser.close();
      }
      console.log("proxy-worker-fetch: /wedge reached interactive in a browser (SSR stream closed, not wedged)");
    }

    console.log("proxy-worker-fetch: worker-originated + browser-direct /go-api proxied and stripped, /wedge not wedged");
  } finally {
    stop();
  }
}

try {
  installCloudflareDeps();
  makeApp();
  await new Promise((r) => upstream.listen(UPSTREAM, "127.0.0.1", r));
  await run();
  upstream.close();
  cleanup();
} catch (e) {
  console.error(e?.stack || e);
  upstream.close();
  if (keep) console.error(`kept ${tmp}`);
  else cleanup();
  process.exit(1);
}
