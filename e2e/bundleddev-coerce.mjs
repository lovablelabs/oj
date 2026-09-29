// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// A TanStack Start app on Cloudflare with experimental.bundledDev enabled, under
// `oj dev`. oj does not implement Vite's Rolldown bundled client dev mode; it
// must coerce experimental.bundledDev off so the app falls back to the standard
// Vite dev client entry oj already serves, and warn once that it did so.
//
// With bundledDev left ON, the app's TanStack manifest emits a `/assets/index.js`
// client script that oj never builds nor serves: `/assets/index.js` HANGS on the
// app's own bundled-dev entry hold and `/bundledDevClient.mjs` 404s, so no client
// JS runs and React never hydrates — the SSR shell paints but the client-only
// content never appears.
//
// This fixture asserts the FIXED behavior: `/` references the standard dev client
// entry (not `/assets/index.js`), that entry serves 200, the one-time warning is
// printed, and a headless browser HYDRATES the page (a useEffect-mounted marker,
// absent from the SSR HTML, appears). Run on an UNFIXED tree it reproduces the
// bug instead: it detects the `/assets/index.js` reference, proves the hang and
// the no-hydration symptom, and FAILS with that evidence.
//
// Deps come from the same install recipe as start-cloudflare-dev.mjs
// (OJ_E2E_CF_DEPS or a fresh npm install). Playwright drives the browser
// assertion; CI installs it under e2e/. When it is not resolvable locally, the
// HTTP-level assertions still run and the browser one is skipped with a note.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { assertHydrates, assertModuleGraphServes, parseBoundPort } from "./lib/hydration.mjs";
import { settles } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const REQ_PORT = 6860;
// The port oj actually bound (it auto-increments off a busy port); read from its
// stdout so a stale server never makes the browser hit the wrong server.
let PORT = REQ_PORT;

const installed =
  fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(fixture, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP bundledDev coerce: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-bundleddev-"));
const app = path.join(tmp, "app");
const keep = !!process.env.OJ_E2E_KEEP;
// Retried: the SIGKILLed server's node children flush caches for a beat and
// race the removal (ENOTEMPTY), like start-cloudflare-dev.mjs handles.
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
  fs.writeFileSync(
    path.join(tmp, "package.json"),
    JSON.stringify({ name: "oj-cf-deps", private: true, type: "module" }),
  );
  execSync("npm install --no-audit --no-fund --no-package-lock @cloudflare/vite-plugin wrangler", {
    cwd: tmp,
    stdio: "inherit",
  });
}

function makeApp() {
  fs.mkdirSync(app);
  for (const f of ["src", "public", "styles", "packages", "tsconfig.json", "package.json"]) {
    fs.cpSync(path.join(fixture, f), path.join(app, f), { recursive: true });
  }
  fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");
  fs.writeFileSync(
    path.join(app, "wrangler.jsonc"),
    [
      "{",
      '  "name": "oj-bundleddev-fixture",',
      '  "main": "@tanstack/react-start/server-entry",',
      '  "compatibility_date": "2025-09-01",',
      '  "compatibility_flags": ["nodejs_compat"],',
      '  "vars": { "EDITION": "fixture-edition" }',
      "}",
      "",
    ].join("\n"),
  );
  const original = fs.readFileSync(path.join(fixture, "vite.config.ts"), "utf8");
  const withImport = original.replace(
    'import { defineConfig } from "vite";',
    'import { defineConfig } from "vite";\nimport { cloudflare } from "@cloudflare/vite-plugin";',
  );
  // The browser hydrates by fetching the client entry's source under
  // node_modules via @fs; this tmp app's node_modules is symlinked to the shared
  // fixture install OUTSIDE the app root, which oj's default fs allow-list
  // rejects. Turn strict off for the harness (a real app keeps node_modules
  // under its root, where the default allow already covers it).
  // Turn on Vite's Rolldown bundled client dev mode — the flag oj must coerce off.
  const withBundledDev = withImport.replace(
    /export default defineConfig\(\{/,
    "export default defineConfig({\n  experimental: { bundledDev: true },\n  server: { fs: { strict: false } },",
  );
  const config = withBundledDev.replace(
    /^(\s*)tanstackStart\(/m,
    '$1cloudflare({ viteEnvironment: { name: "ssr" } }),\n$1tanstackStart(',
  );
  if (config === original || config === withImport || config === withBundledDev) {
    throw new Error("fixture vite.config.ts changed shape; update this script");
  }
  fs.writeFileSync(path.join(app, "vite.config.ts"), config);
  // Slimmed like start-cloudflare-dev.mjs: the full-fat index route does not run
  // under the plugin's own worker pipeline. This test is about the CLIENT path —
  // whether the coerced-off config lets the browser hydrate — so the route is
  // deliberately backend-free: no loader and no server function, because a
  // client-side server-function call would run in oj's Node SSR loader, which
  // cannot import `cloudflare:workers` (the slim-app limitation start-cloudflare-
  // dev.mjs documents). The `client-mounted-ok` span is rendered only AFTER the
  // component mounts, so it is absent from the SSR HTML and its appearance in the
  // browser proves React hydrated — the exact thing the bundledDev bug broke.
  // The counter button proves event handlers attached post-hydration: it starts
  // at 0 server-side and only increments once the client runtime is live and its
  // onClick is wired (the interaction step of the shared hydration gate).
  fs.writeFileSync(
    path.join(app, "src", "routes", "index.tsx"),
    [
      'import { createRoute } from "@tanstack/react-router";',
      'import { useEffect, useState } from "react";',
      'import { rootRoute } from "./__root";',
      'import { shout } from "#lib/format";',
      "",
      "export const indexRoute = createRoute({",
      "  getParentRoute: () => rootRoute,",
      '  path: "/",',
      "  component: Index,",
      "});",
      "",
      "function Index() {",
      "  const [mounted, setMounted] = useState(false);",
      "  const [count, setCount] = useState(0);",
      "  useEffect(() => setMounted(true), []);",
      "  return (",
      "    <main>",
      '      <h1 className="fixture-heading">{shout("home")}</h1>',
      '      {mounted ? <span data-testid="client-mounted">client-mounted-ok</span> : null}',
      '      <button data-testid="counter" onClick={() => setCount((c) => c + 1)}>count: {count}</button>',
      "    </main>",
      "  );",
      "}",
      "",
    ].join("\n"),
  );
}

const req = async (route, { timeoutMs } = {}) => {
  const ac = timeoutMs ? AbortSignal.timeout(timeoutMs) : undefined;
  const res = await fetch(`http://127.0.0.1:${PORT}${route}`, { signal: ac });
  return { status: res.status, body: await res.text() };
};

const STANDARD_ENTRY = "virtual:tanstack-start-dev-client-entry";
const BUNDLED_ENTRY = "/assets/index.js";

async function loadPlaywright() {
  try {
    const { chromium } = await import("playwright");
    return chromium;
  } catch {
    return null;
  }
}

// A client→server-function call can't run in oj's Node SSR loader on this slim
// CF app (it can't import `cloudflare:workers`); this route is deliberately
// backend-free, so nothing here trips it, but the whitelist documents the known
// noise the shared gate should ignore.
const HYDRATION_WHITELIST = [/favicon\.ico/, "cloudflare:workers"];

async function run() {
  let log = "";
  const srv = spawn(oj, ["dev", app, "--port", String(REQ_PORT)], {
    cwd: app,
    detached: true,
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, WRANGLER_SEND_METRICS: "false", CI: "1", NO_COLOR: "1", FORCE_COLOR: "0" },
  });
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
  try {
    // Read the port oj actually bound before probing it: it may have incremented
    // off REQ_PORT, and probing REQ_PORT would then hit a stale server.
    await settles(
      () => {
        if (srv.exitCode != null) return true;
        const bound = parseBoundPort(log);
        if (bound) PORT = bound;
        return Boolean(bound);
      },
      { timeoutMs: 60000, pollMs: 250 },
    );
    let up = false;
    let lastStatus = "no response";
    await settles(
      async () => {
        if (srv.exitCode != null) return true;
        try {
          const r = await fetch(`http://127.0.0.1:${PORT}/`);
          lastStatus = String(r.status);
          up = r.status === 200;
          if (!up) lastStatus = `${r.status}: ${(await r.text()).slice(0, 300)}`;
        } catch (e) {
          lastStatus = `fetch error: ${(e && e.message) || e}`;
        }
        return up;
      },
      { timeoutMs: 120000, pollMs: 500 },
    );
    if (!up) throw new Error(`oj dev did not serve 200 on :${PORT} (last: ${lastStatus}); log:\n${log.slice(-4000)}`);

    const home = await req("/");
    if (home.status !== 200) throw new Error(`/ returned ${home.status}`);
    // The route rendered server-side (shout("home") -> "HOME!").
    if (!home.body.includes("HOME!")) {
      throw new Error(`/: route did not render server-side (no "HOME!")\n${home.body.slice(0, 1500)}`);
    }
    // Sanity: the client-only marker must NOT be in the SSR HTML — its later
    // appearance in the browser is what proves hydration.
    if (home.body.includes("client-mounted-ok")) {
      throw new Error("the client-only marker leaked into the SSR HTML; it can no longer prove hydration");
    }

    const chromium = await loadPlaywright();
    const base = `http://127.0.0.1:${PORT}`;
    const referencesBundled = home.body.includes(BUNDLED_ENTRY);

    if (referencesBundled) {
      // ---- UNFIXED TREE: reproduce the bug and FAIL with evidence. ----
      // This is the branch a tree with the bundledDev coercion REVERTED lands in.
      console.error("bundledDev-coerce: REPRODUCED the bug (document references /assets/index.js)");
      // The bundled client script is never delivered: on Vite it hangs on the
      // app's own entry hold; under oj (which never installs the bundled-dev
      // middlewares) the route simply does not exist. Either way it never
      // completes with a 200, so no client JS ever runs.
      let noClientJs = false;
      try {
        const res = await req(BUNDLED_ENTRY, { timeoutMs: 5000 });
        noClientJs = res.status !== 200;
        console.error(`  /assets/index.js returned ${res.status} (not a delivered client bundle) — no client JS`);
      } catch {
        noClientJs = true;
        console.error("  /assets/index.js HANGS within 5s (no client JS is ever delivered) — confirmed");
      }
      if (chromium) {
        // The shared hydration gate surfaces the same failure at step 2 (the
        // client module graph served >= 400) rather than a bare timeout.
        const browser = await chromium.launch();
        try {
          const { failures } = await assertHydrates(browser, `${base}/`, {
            ssrMarker: "HOME!",
            clientMarker: '[data-testid="client-mounted"]',
            deadlineMs: 8000,
            whitelist: HYDRATION_WHITELIST,
            throwOnFail: false,
          });
          console.error("  hydration gate failures:\n    " + failures.join("\n    "));
        } finally {
          await browser.close();
        }
      } else {
        console.error("  (playwright not installed locally; skipped the browser no-hydration check — CI covers it)");
      }
      throw new Error(
        `experimental.bundledDev was not coerced off: document still references ${BUNDLED_ENTRY} (client bundle undelivered=${noClientJs}). ` +
          "This is the failing repro; the fix must coerce it to the standard dev client entry.",
      );
    }

    // ---- FIXED TREE: assert the standard entry + hydration + warning. ----
    if (!home.body.includes(STANDARD_ENTRY)) {
      throw new Error(
        `/: references neither ${BUNDLED_ENTRY} nor the standard entry ${STANDARD_ENTRY}\n${home.body.slice(0, 1500)}`,
      );
    }
    console.log(
      `bundledDev-coerce: / references the standard dev client entry (${STANDARD_ENTRY}), not ${BUNDLED_ENTRY}`,
    );

    // The standard dev client entry must actually serve.
    const entry = await req(`/@id/${STANDARD_ENTRY}`);
    if (entry.status !== 200) {
      throw new Error(
        `the standard dev client entry did not serve 200 (got ${entry.status})\n${entry.body.slice(0, 800)}`,
      );
    }
    console.log("bundledDev-coerce: the standard dev client entry serves 200");

    // The one-time coercion warning must have been printed.
    const warned = await settles(() => /experimental\.bundledDev is not supported/.test(log));
    if (!warned) {
      throw new Error(`the one-time bundledDev coercion warning was never printed; log tail:\n${log.slice(-2000)}`);
    }
    console.log("bundledDev-coerce: the one-time coercion warning was printed");

    // The gates the log-greps missed: the whole client module graph must serve,
    // and a real browser must HYDRATE. assertModuleGraphServes follows the
    // virtual entry to the physical client.tsx behind it, so a 500 there (the
    // react-refresh-wrapper regression) surfaces as a named `500 …/client.tsx`,
    // not a silent waitForSelector timeout.
    if (chromium) {
      const browser = await chromium.launch();
      try {
        const page = await browser.newPage();
        try {
          const graph = await assertModuleGraphServes(page, `${base}/@id/${STANDARD_ENTRY}`);
          if (!graph.ok) {
            throw new Error(
              "the dev client entry's module graph did not fully serve 200:\n  " + graph.failures.join("\n  "),
            );
          }
          console.log("bundledDev-coerce: the client entry module graph (incl. the physical client.tsx) serves 200");
        } finally {
          await page.close();
        }
        // Full ladder: SSR marker, client mount, correct marker text, and an
        // interaction (the counter) proving handlers attached post-hydration.
        await assertHydrates(browser, `${base}/`, {
          ssrMarker: "HOME!",
          clientMarker: '[data-testid="client-mounted"]',
          clientMarkerText: "client-mounted-ok",
          interaction: {
            click: '[data-testid="counter"]',
            expect: { selector: '[data-testid="counter"]', text: "count: 1" },
          },
          deadlineMs: 30000,
          whitelist: HYDRATION_WHITELIST,
        });
        console.log(
          "bundledDev-coerce: headless Chromium HYDRATED (marker + counter interaction) — client runtime is live",
        );
      } finally {
        await browser.close();
      }
    } else {
      console.log(
        "bundledDev-coerce: playwright not installed locally; ran the HTTP-level assertions and skipped the browser hydration check (CI runs it)",
      );
    }
  } finally {
    stop();
  }
}

try {
  installCloudflareDeps();
  makeApp();
  await run();
  cleanup();
  console.log("bundledDev-coerce: PASS");
} catch (e) {
  console.error(e?.stack || e);
  if (keep) console.error(`kept ${tmp}`);
  else cleanup();
  process.exit(1);
}
