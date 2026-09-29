// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// A TanStack Start app on Cloudflare under `oj dev`: with @cloudflare/vite-plugin
// in the config the plugin's worker (Miniflare/workerd) renders the documents.
// This drives the edit loop end to end: boot, render a route in the worker,
// verify the live-reload client is injected into the worker-served document,
// edit a route file and require the next document to be fresh (the targeted
// HMR invalidation of the worker environments). Deps come from the same
// install recipe as start-cloudflare.mjs (or OJ_E2E_CF_DEPS).

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { assertHydrates, parseBoundPort } from "./lib/hydration.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const REQ_PORT = 6840;
// The port oj actually bound (it auto-increments off a busy port); read from its
// stdout so a stale server never makes the browser hit the wrong server.
let PORT = REQ_PORT;

const installed =
  fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(fixture, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP start cloudflare dev: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-cf-dev-"));
const app = path.join(tmp, "app");
const keep = !!process.env.OJ_E2E_KEEP;
// Retried: the SIGKILLed server's node children flush caches for a beat and
// race the removal (ENOTEMPTY), like start.mjs handles.
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
  fs.mkdirSync(app);
  for (const f of ["src", "public", "styles", "packages", "tsconfig.json", "package.json"]) {
    fs.cpSync(path.join(fixture, f), path.join(app, f), { recursive: true });
  }
  fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");
  fs.writeFileSync(path.join(app, "wrangler.jsonc"), [
    "{",
    '  "name": "oj-start-fixture",',
    '  "main": "@tanstack/react-start/server-entry",',
    '  "compatibility_date": "2025-09-01",',
    '  "compatibility_flags": ["nodejs_compat"],',
    '  "vars": { "EDITION": "fixture-edition" }',
    "}",
    "",
  ].join("\n"));
  const original = fs.readFileSync(path.join(fixture, "vite.config.ts"), "utf8");
  const withImport = original.replace(
    'import { defineConfig } from "vite";',
    'import { defineConfig } from "vite";\nimport { cloudflare } from "@cloudflare/vite-plugin";',
  );
  const config = withImport.replace(/^(\s*)tanstackStart\(/m, '$1cloudflare({ viteEnvironment: { name: "ssr" } }),\n$1tanstackStart(');
  if (config === original || config === withImport) throw new Error("fixture vite.config.ts changed shape; update this script");
  fs.writeFileSync(path.join(app, "vite.config.ts"), config);
  // The dev worker path runs the app through the app's OWN Vite pipeline into
  // workerd, where the full fixture index route does not run even under
  // `vite dev` + the plugin (its CommonJS dep fails in the ESM module runner,
  // and the tsconfig-paths alias has no plugin support). Slim the index route
  // to what this test drives: a worker render with a server function reading a
  // wrangler var. The full-fat route stays covered by start.mjs (Node runner)
  // and start-cloudflare.mjs (prod worker build).
  // The mount probe + counter give the browser hydration gate a client-only
  // marker (absent from SSR HTML) and a post-hydration interaction, so the CF
  // worker-render path is proven to HYDRATE, not just to emit SSR HTML. The
  // counter is pure client state (no server call), so it does not trip the CF
  // slim-app server-fn limitation.
  fs.writeFileSync(path.join(app, "src", "routes", "index.tsx"), [
    'import { createRoute, useLoaderData } from "@tanstack/react-router";',
    'import { useEffect, useState } from "react";',
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
    "  const [mounted, setMounted] = useState(false);",
    "  const [count, setCount] = useState(0);",
    "  useEffect(() => setMounted(true), []);",
    "  return (",
    "    <main>",
    '      <h1 className="fixture-heading">{shout("home")}</h1>',
    '      <p data-testid="server-fn">{data.message} / edition={data.edition}</p>',
    '      {mounted ? <span data-testid="client-mounted">client-mounted-ok</span> : null}',
    '      <button data-testid="counter" onClick={() => setCount((c) => c + 1)}>count: {count}</button>',
    "    </main>",
    "  );",
    "}",
    "",
  ].join("\n"));
  // The fixture reads the wrangler var through `@cloudflare/vite-plugin/server`,
  // an alias oj itself provides; in the plugin's own worker pipeline the real
  // API is the `cloudflare:workers` env import.
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
}

const get = async (route) => {
  const res = await fetch(`http://127.0.0.1:${PORT}${route}`);
  return { status: res.status, body: await res.text() };
};

async function loadPlaywright() {
  try {
    return (await import("playwright")).chromium;
  } catch {
    return null;
  }
}

async function runDev() {
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
    // Read the port oj actually bound before probing it (it may increment off a
    // busy REQ_PORT), so a stale server never fools the probes or the browser.
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

    // The document must have rendered in the worker (the wrangler var proves
    // it) and must carry the live-reload client even though the plugin
    // middleware served it.
    const home = await get("/");
    if (home.status !== 200) throw new Error(`/ returned ${home.status}`);
    for (const [what, marker] of [
      ["home render", "HOME!"],
      ["worker render (wrangler var)", "fixture-edition"],
      ["server function", "server-fn-marker"],
    ]) {
      if (!home.body.includes(marker)) throw new Error(`/: missing ${what} ("${marker}")\n${home.body.slice(0, 1500)}`);
    }
    if (!home.body.includes("/@oj-start/live-reload.js")) {
      throw new Error("worker-served document lacks the live-reload client");
    }

    const about = await get("/about");
    if (about.status !== 200 || !about.body.includes("about-page-marker")) {
      throw new Error(`/about did not render (${about.status})`);
    }

    // The CF worker-render path must HYDRATE, not just emit SSR HTML: load `/`
    // in a real browser, prove the client module graph serves and the runtime
    // mounts + responds. Whitelist the documented CF slim-app limitation (a
    // client→server-fn call can't run in oj's Node SSR loader, which can't
    // import cloudflare:workers) plus the incidental favicon 404. Run before the
    // edit loop mutates HOME! into HOME-RAPID!.
    const chromium = await loadPlaywright();
    if (chromium) {
      const browser = await chromium.launch();
      try {
        await assertHydrates(browser, `http://127.0.0.1:${PORT}/`, {
          ssrMarker: "server-fn-marker",
          clientMarker: '[data-testid="client-mounted"]',
          clientMarkerText: "client-mounted-ok",
          interaction: { click: '[data-testid="counter"]', expect: { selector: '[data-testid="counter"]', text: "count: 1" } },
          whitelist: [/favicon\.ico/, "cloudflare:workers", "/_serverFn/", "createServerFn"],
        });
        console.log("start-cloudflare-dev: browser hydration ok (worker render mounts + counter interaction)");
      } catch (e) {
        // A browser-observed 500/crash in oj's own pipeline leaves its cause
        // only in oj's server log; surface a bounded tail on the thrown error
        // so CI names the compile/loader failure instead of a bare status.
        // Guard the message mutation: a non-Error throw (a string, say) has no
        // writable .message, and clobbering it would hide the real failure.
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
      console.log("start-cloudflare-dev: playwright not installed locally; skipped the browser hydration pass (CI runs it)");
    }

    // The edit loop: change the route, require the next document to be fresh.
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

    // Rapid edits: a second edit landing while the first edit's client
    // rebundle is in flight must not queue behind it. Its worker invalidate
    // fires promptly, so the next document is fresh well under the ~4.6s the
    // old serialized loop took.
    const indexTarget = path.join(app, "src", "routes", "index.tsx");
    fs.writeFileSync(
      indexTarget,
      fs.readFileSync(indexTarget, "utf8").replace('shout("home")', 'shout("home-rapid")'),
    );
    await new Promise((r) => setTimeout(r, 300));
    fs.writeFileSync(
      target,
      fs.readFileSync(target, "utf8").replace("about-page-edited-marker", "about-page-rapid-marker"),
    );
    const t1 = Date.now();
    let rapid = null;
    // A generous deadline: the machine is saturated by the first edit's
    // rebundle here, so a loaded box can take a while even though the prompt
    // invalidate usually lands well under a second. The measured elapsed ms is
    // printed below, so the fast path stays visible in the logs.
    for (let i = 0; i < 300; i++) {
      const res = await get("/about");
      if (res.status === 200 && res.body.includes("about-page-rapid-marker")) {
        rapid = Date.now() - t1;
        break;
      }
      await new Promise((r) => setTimeout(r, 50));
    }
    if (rapid == null) {
      throw new Error(`/about still stale 15s after the rapid second edit; log tail:\n${log.slice(-4000)}`);
    }

    // The coalesced rebundle must not lose the first edit either.
    let firstEditFresh = false;
    for (let i = 0; i < 100 && !firstEditFresh; i++) {
      const res = await get("/");
      firstEditFresh = res.status === 200 && res.body.includes("HOME-RAPID!");
      if (!firstEditFresh) await new Promise((r) => setTimeout(r, 100));
    }
    if (!firstEditFresh) {
      throw new Error(`/ never picked up the first rapid edit; log tail:\n${log.slice(-4000)}`);
    }

    // A new route file: the run regenerates routeTree.gen.ts, and the worker
    // environments must be invalidated for the regenerated output AFTER the
    // regen (the watcher deliberately never forwards routeTree.gen events).
    // This fixture's router is code-based (src/routeTree.ts), so the generated
    // tree is never imported by the app and the new route cannot serve;
    // instead assert through the logs that oj sent the post-regen invalidate
    // and the plugin host received it (the graph not knowing routeTree.gen.ts
    // makes the host log the unmatched change, which doubles as a receipt).
    // Retouchable (identical bytes): the watcher thread may register watches
    // after HTTP-ready on a loaded runner, so a single write can predate
    // registration and never be seen; the mtime bump re-fires it.
    const extraRoute = [
      'import { createFileRoute } from "@tanstack/react-router";',
      "",
      'export const Route = createFileRoute("/extra")({',
      "  component: () => <main>extra-page-marker</main>,",
      "});",
      "",
    ].join("\n");
    const touchExtra = () => fs.writeFileSync(path.join(app, "src", "routes", "extra.tsx"), extraRoute);
    touchExtra();
    let regenSent = false;
    let regenReceived = false;
    for (let i = 0; i < 450 && !(regenSent && regenReceived); i++) {
      regenSent = /regen outputs changed \([^)]*routeTree\.gen\.ts/.test(log);
      regenReceived = /routeTree\.gen\.ts matched no module/.test(log);
      if (i > 0 && i % 30 === 0 && !regenSent) touchExtra();
      await new Promise((r) => setTimeout(r, 100));
    }
    if (!regenSent) {
      throw new Error(`no post-regen worker invalidate for routeTree.gen.ts within 45s of a new route file; log tail:\n${log.slice(-4000)}`);
    }
    if (!regenReceived) {
      throw new Error(`the plugin host never received the routeTree.gen.ts invalidate; log tail:\n${log.slice(-4000)}`);
    }
    // The generator run must not have broken the app.
    const after = await get("/");
    if (after.status !== 200 || !after.body.includes("HOME-RAPID!")) {
      throw new Error(`/ broken after the new-route regen (${after.status}); log tail:\n${log.slice(-4000)}`);
    }

    console.log(
      `start-cloudflare-dev: worker render + live-reload client + fresh document ${fresh}ms after the edit, ${rapid}ms after a rapid second edit, post-regen routeTree invalidate observed`,
    );
  } finally {
    stop();
  }
}

// Slow-boot phases: a plugin host whose top-level init is slow (74-plugin
// fleets, a Miniflare boot inside configureServer) used to burn every boot RPC's
// own timeout and leave the worker path silently inactive — no "plugin
// middleware: forwarding" line, every document degraded to the Node SSR runner
// for the whole session. Two fixes, each with its own phase below:
//   1. RPC sends are init-gated (bounded by OJ_PLUGIN_INIT_TIMEOUT, default
//      generous): a slow boot now blocks briefly-but-correctly, so the
//      forwarding line prints normally, with no late-activation note.
//   2. Should the init deadline still pass (a genuinely wedged-looking boot),
//      the host's { ojServeInfo } push activates the middleware late, with a
//      catch-up resync so edits from the degraded window are not lost.
function addSlowBootPlugin() {
  const cfg = path.join(app, "vite.config.ts");
  const original = fs.readFileSync(cfg, "utf8");
  const patched = original.replace(
    /^(\s*)cloudflare\(/m,
    '$1{ name: "slow-boot-probe", async configureServer() { await new Promise((r) => setTimeout(r, 8000)); } },\n$1cloudflare(',
  );
  if (patched === original) throw new Error("vite.config.ts changed shape; update this script");
  fs.writeFileSync(cfg, patched);
}

function spawnSlowDev(port, extraEnv) {
  const srv = spawn(oj, ["dev", app, "--port", String(port)], {
    cwd: app,
    detached: true,
    stdio: ["ignore", "pipe", "pipe"],
    env: {
      ...process.env,
      WRANGLER_SEND_METRICS: "false",
      CI: "1",
      NO_COLOR: "1",
      FORCE_COLOR: "0",
      ...extraEnv,
    },
  });
  const state = { log: "" };
  srv.stdout.on("data", (d) => (state.log += d));
  srv.stderr.on("data", (d) => (state.log += d));
  const stop = async () => {
    try { process.kill(-srv.pid, "SIGTERM"); } catch {}
    setTimeout(() => { try { process.kill(-srv.pid, "SIGKILL"); } catch {} }, 2000).unref();
    // Let the SIGTERM land before the next phase (or cleanup) touches the tree.
    await new Promise((r) => setTimeout(r, 500));
  };
  const waitForLog = async (re, what, tries = 240) => {
    for (let i = 0; i < tries; i++) {
      if (re.test(state.log)) return;
      if (srv.exitCode != null) break;
      await new Promise((r) => setTimeout(r, 500));
    }
    throw new Error(`${what}; log tail:\n${state.log.slice(-4000)}`);
  };
  const waitForDoc = async (route, marker, what, tries = 120) => {
    for (let i = 0; i < tries; i++) {
      try {
        const res = await fetch(`http://127.0.0.1:${port}${route}`);
        const body = await res.text();
        if (res.status === 200 && body.includes(marker)) return body;
      } catch {}
      await new Promise((r) => setTimeout(r, 500));
    }
    throw new Error(`${what}; log tail:\n${state.log.slice(-4000)}`);
  };
  return { srv, state, stop, waitForLog, waitForDoc };
}

// Their own ports: the previous phase's server may still be tearing down and
// oj would silently bind PORT+1, leaving the probes pointed at a dead socket.
const GATED_PORT = REQ_PORT + 3;
const DEGRADED_PORT = REQ_PORT + 4;

// Phase 1: the default init deadline. Boot blocks on the 8s init instead of
// racing per-RPC timeouts against it: the forwarding line prints WITHOUT the
// late-activation note, and the worker renders — no degradation at all.
async function runSlowBootGatedDev() {
  const { state, stop, waitForLog, waitForDoc } = spawnSlowDev(GATED_PORT, {});
  try {
    await waitForLog(
      /forwarding unmatched requests to :\d+/,
      "the forwarding line never printed on the init-gated slow boot",
    );
    if (/host came up after boot/.test(state.log)) {
      throw new Error(`init gating must make the slow boot block, not activate late; log tail:\n${state.log.slice(-4000)}`);
    }
    if (/still initializing after \d+s running/.test(state.log)) {
      throw new Error(`boot RPCs must not blow their deadline under init gating; log tail:\n${state.log.slice(-4000)}`);
    }
    const home = await waitForDoc("/", "fixture-edition", "no worker-rendered document after the init-gated slow boot");
    if (!home.includes("/@oj-start/live-reload.js")) {
      throw new Error("worker document lacks the live-reload client after the init-gated slow boot");
    }
    console.log("start-cloudflare-dev: slow boot (init-gated) — boot blocked correctly, no degradation");
  } finally {
    await stop();
  }
}

// Phase 2: the init deadline shrunk under the 8s init, forcing the degraded
// window. The middleware must activate late off the push, resync the worker
// environments, and an edit made DURING the window must be reflected after.
async function runSlowBootDegradedDev() {
  const { state, stop, waitForLog, waitForDoc } = spawnSlowDev(DEGRADED_PORT, {
    // Shrink the init deadline so the 8s configureServer sleep reliably
    // outlives it, the way a wedged boot outlives the default.
    OJ_PLUGIN_INIT_TIMEOUT: "2",
  });
  try {
    // The degraded window opened: oj said so out loud (finding-6 semantics —
    // the warning keys off the host never initializing, not config text).
    await waitForLog(
      /plugin host did not finish initializing within 2s/,
      "the init-deadline warning never printed",
    );

    // An edit DURING the degraded window: no middleware port exists yet, so no
    // invalidate can be sent — the post-activation resync must cover it.
    const target = path.join(app, "src", "routes", "about.tsx");
    fs.writeFileSync(
      target,
      fs.readFileSync(target, "utf8").replace("about-page-rapid-marker", "about-page-lateboot-marker"),
    );

    // The middleware activates late off the host's ojServeInfo push, and the
    // catch-up resync full-reloads the worker environments.
    await waitForLog(
      /forwarding unmatched requests to :\d+ \(host came up after boot\)/,
      "the worker path never activated after the degraded slow boot",
    );
    await waitForLog(/worker environments resynced/, "no catch-up resync after late activation");

    // The documents must then render in the worker (wrangler var proves it),
    // with the live-reload client, and reflect the degraded-window edit.
    const home = await waitForDoc("/", "fixture-edition", "no worker-rendered document after late activation");
    if (!home.includes("/@oj-start/live-reload.js")) {
      throw new Error("late-activated worker document lacks the live-reload client");
    }
    await waitForDoc(
      "/about",
      "about-page-lateboot-marker",
      "the degraded-window edit never reached the worker after the resync",
    );
    console.log("start-cloudflare-dev: slow boot (degraded) — late activation + resync, degraded-window edit fresh");
  } finally {
    await stop();
  }
}

try {
  installCloudflareDeps();
  makeApp();
  await runDev();
  addSlowBootPlugin();
  await runSlowBootGatedDev();
  await runSlowBootDegradedDev();
  cleanup();
} catch (e) {
  console.error(e?.stack || e);
  if (keep) console.error(`kept ${tmp}`);
  else cleanup();
  process.exit(1);
}
