// SPDX-License-Identifier: MIT
// Solid Start e2e (TanStack Solid Start app, no plain Solid, no React).
// Shape mirrors e2e/start.mjs (React): raw dev + prod SSR assertions, head
// ordering, browser hydration + reactivity, routing, and an edit/rebuild gate.
// The raw body assertions deliberately exclude serialized scripts/head: an
// empty SSR body with client-masked rendering (the known dev gap) must FAIL.
// Run: `node e2e/solid-start.mjs` (or with --dev / --prod).

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { assertHydrates, parseBoundPort } from "./lib/hydration.mjs";
import { assertDeepestTitle, bodyWithoutScripts, stripScripts } from "./lib/solid-start-html.mjs";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const app = path.join(here, "fixtures", "solid-start-app");
const oj = path.join(repo, "target", "debug", "oj");
const onlyDev = process.argv.includes("--dev");
const onlyProd = process.argv.includes("--prod");
const runDev = !onlyProd;
const runProd = !onlyDev;

const installed =
  fs.existsSync(path.join(app, "node_modules", "@tanstack", "solid-start")) &&
  fs.existsSync(path.join(app, "node_modules", "solid-js"));
if (!installed) {
  console.log("SKIP solid-start integration: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/solid-start-app && bun install)");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
// Retried: SIGKILLed servers orphan node children for a beat, and their
// NODE_COMPILE_CACHE flush into .oj-cache/v8 races the removal (ENOTEMPTY).
const rm = (p) => {
  for (let i = 0; ; i++) {
    try {
      return fs.rmSync(p, { recursive: true, force: true });
    } catch (e) {
      if (i >= 20) throw e;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100);
    }
  }
};

const get = async (port, route = "/") => {
  // Per-request timeout so a hung SSR render bounds settles()/restore polls
  // instead of wedging them on one fetch+body read. Restored (cleared) on
  // every return/throw path via finally.
  const REQUEST_TIMEOUT_MS = 15000;
  const ctl = new AbortController();
  const timer = setTimeout(() => ctl.abort(), REQUEST_TIMEOUT_MS);
  try {
    const res = await fetch(`http://localhost:${port}${route}`, { signal: ctl.signal });
    return { status: res.status, body: await res.text() };
  } finally {
    clearTimeout(timer);
  }
};

// Kill a spawned server and wait for its exit before cache/dist deletions:
// a SIGKILLed oj server orphans node children for a beat and their
// NODE_COMPILE_CACHE flush races the removal (ENOTEMPTY), so callers must
// await this (not fire-and-forget kill) before rm().
const killAndWait = (proc) =>
  new Promise((resolve) => {
    if (!proc || proc.exitCode !== null) return resolve();
    const done = () => {
      clearTimeout(timer);
      resolve();
    };
    const timer = setTimeout(done, 5000);
    proc.once("exit", done);
    try {
      proc.kill("SIGKILL");
    } catch {
      done();
    }
  });

// SSR HTML helpers (bodyWithoutScripts/headOf/assertDeepestTitle) live in
// e2e/lib/solid-start-html.mjs: scripts are stripped case-insensitively from
// the ENTIRE document BEFORE body/head boundaries are located, so a fake
// <body>/<title> inside a head <script> payload can never satisfy a healthy
// route assertion. Boundaries are required (no whole-document fallback);
// focused unit coverage is e2e/unit/solid-start-html.test.mjs.

async function assertApp(port, label) {
  const home = await get(port, "/");
  if (home.status !== 200) throw new Error(`${label}: / returned ${home.status}`);
  const h = home.body;
  // h1/p markers asserted in the actual script-stripped body: the marker must
  // be inside its element, not in <head> or serialized script data.
  const body = bodyWithoutScripts(h);
  if (!/<h1[^>]*>\s*solid-start-home-marker\s*<\/h1>/.test(body)) {
    throw new Error(`${label}: / SSR body missing the home <h1> marker`);
  }
  if (!/<p[^>]*data-testid="hero"[^>]*>\s*solid-start-hero-marker\s*<\/p>/.test(body)) {
    throw new Error(`${label}: / SSR body missing the imported hero <p> marker`);
  }
  if (!/<p[^>]*>[^<]*solid-start-raw-notes-marker[^<]*<\/p>/.test(body)) {
    throw new Error(`${label}: / SSR body missing the ?raw notes <p> marker`);
  }
  assertDeepestTitle(h, label, "/", "solid-start-fixture-home");

  const about = await get(port, "/about");
  if (about.status !== 200) throw new Error(`${label}: /about returned ${about.status}`);
  const aboutBody = bodyWithoutScripts(about.body);
  if (!/<h1[^>]*>\s*solid-start-about-marker\s*<\/h1>/.test(aboutBody)) {
    throw new Error(`${label}: /about SSR body missing the about <h1> marker`);
  }
  if (!/<p[^>]*>[^<]*solid-start-about-copy-marker[^<]*<\/p>/.test(aboutBody)) {
    throw new Error(`${label}: /about SSR body missing the about <p> marker`);
  }
  assertDeepestTitle(about.body, label, "/about", "solid-start-fixture-about");

  // /boom is a LOADER throw (caught before first render): the framework
  // renders the route errorComponent server-side. The reference answers 500
  // while a handled loader throw may legitimately be 200, so either status
  // passes — the boundary message (including the thrown text) is the assert.
  const boom = await get(port, "/boom");
  if (boom.status !== 200 && boom.status !== 500) {
    throw new Error(`${label}: /boom returned ${boom.status} (want the handled 200 or 500)`);
  }
  const boomBody = bodyWithoutScripts(boom.body);
  if (!/<h1[^>]*>\s*solid-start-boom-error-marker\s*<\/h1>/.test(boomBody)) {
    throw new Error(`${label}: /boom did not render the error boundary marker`);
  }
  if (!boomBody.includes("solid-start-boom-thrown-marker")) {
    throw new Error(`${label}: /boom error boundary lost the thrown message`);
  }
  if (boomBody.includes("solid-start-home-marker")) {
    throw new Error(`${label}: /boom leaked home content into the error render`);
  }

  // /render-boom is an actual COMPONENT render throw with NO custom
  // errorComponent, so it reaches the global default boundary. That boundary
  // renders a legitimate fragment ("Something went wrong!", plus the thrown
  // message in dev) with no <body> wrapper on both Vite and oj — so assert
  // the script-stripped whole document (stripScripts, no body-boundary
  // requirement) for the genuine fallback markup, with status 200|500. Do
  // NOT require the healthy-body wrapper here; a fragment is correct.
  const renderBoom = await get(port, "/render-boom");
  if (renderBoom.status !== 200 && renderBoom.status !== 500) {
    throw new Error(`${label}: /render-boom returned ${renderBoom.status} (want the handled 200 or 500)`);
  }
  const renderBoomDoc = stripScripts(renderBoom.body);
  if (!renderBoomDoc.includes("Something went wrong!")) {
    throw new Error(`${label}: /render-boom did not render the global default fallback ("Something went wrong!")`);
  }
  const isDev = label.includes("-dev");
  if (isDev && !renderBoomDoc.includes("solid-start-render-boom-thrown-marker")) {
    throw new Error(`${label}: /render-boom fallback lost the thrown message in dev`);
  }
  if (!isDev && !renderBoomDoc.includes("Something went wrong!")) {
    throw new Error(`${label}: /render-boom prod fallback is not meaningful`);
  }
  if (renderBoomDoc.includes("solid-start-home-marker")) {
    throw new Error(`${label}: /render-boom leaked home content into the error render`);
  }

  const asset = await get(port, "/favicon.txt");
  if (!asset.body.includes("solid-start-public-dir-marker")) {
    throw new Error(`${label}: publicDir asset not served`);
  }

  console.log(`${label}: raw SSR body (h1/p) + head ordering + /about + /boom loader boundary + /render-boom default fallback + publicDir ok`);
}

async function loadPlaywright() {
  try {
    return (await import("playwright")).chromium;
  } catch {
    return null;
  }
}

// Router Link SPA navigation: clicking the root nav must swap routes
// client-side (URL + content change, no document reload) with no page errors.
// The gate waits for the actual onMount probe (client-mounted-ok) before
// starting, then plants a window sentinel bound to the live document and
// asserts it persists across both Link clicks (a full reload would drop the
// sentinel and replace the document), plus URL/body/title checks per leg.
async function assertSpaNavigation(browser, base, label, whitelist) {
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push((e && e.message) || String(e)));
  try {
    await page.goto(`${base}/`, { waitUntil: "domcontentloaded" });
    // The client runtime really mounted (onMount probe), not just SSR paint.
    await page.waitForSelector('[data-testid="client-mounted"]', { timeout: 30000 });
    await page.waitForSelector('[data-testid="nav-about"]');
    // Sentinel + document identity: survives SPA swaps, dies on full reload.
    await page.evaluate(() => {
      window.__solidSpaSentinel = `spa-alive-${Math.random()}`;
      window.__solidSpaDoc = document;
    });
    const sentinelBefore = await page.evaluate(() => window.__solidSpaSentinel);
    const assertNoReload = async (leg) => {
      const sentinelAfter = await page.evaluate(() => window.__solidSpaSentinel);
      if (sentinelAfter !== sentinelBefore) {
        throw new Error(`${label}: SPA navigation ${leg} looks like a full reload (window sentinel lost)`);
      }
      const sameDoc = await page.evaluate(() => window.__solidSpaDoc === document);
      if (!sameDoc) {
        throw new Error(`${label}: SPA navigation ${leg} replaced the document (full reload, not client routing)`);
      }
    };
    await page.click('[data-testid="nav-about"]');
    await page.waitForURL("**/about", { timeout: 30000 });
    await assertNoReload("/ -> /about");
    if (page.url().endsWith("/") || !page.url().includes("/about")) {
      throw new Error(`${label}: SPA navigation URL did not become /about (got ${page.url()})`);
    }
    const aboutH1 = await page.textContent("h1");
    if (!aboutH1 || !aboutH1.includes("solid-start-about-marker")) {
      throw new Error(`${label}: SPA navigation to /about did not render the about content`);
    }
    const aboutTitle = await page.title();
    if (!aboutTitle.includes("solid-start-fixture-about")) {
      throw new Error(`${label}: SPA navigation to /about did not update the title (got ${JSON.stringify(aboutTitle)})`);
    }
    await page.click('[data-testid="nav-home"]');
    await page.waitForURL(`${base}/`, { timeout: 30000 });
    await assertNoReload("/about -> /");
    const homeH1 = await page.textContent("h1");
    if (!homeH1 || !homeH1.includes("solid-start-home-marker")) {
      throw new Error(`${label}: SPA navigation back to / did not render the home content`);
    }
    const homeTitle = await page.title();
    if (!homeTitle.includes("solid-start-fixture-home")) {
      throw new Error(`${label}: SPA navigation back to / did not restore the title (got ${JSON.stringify(homeTitle)})`);
    }
    const live = errors.filter((s) => !whitelist.some((w) => (w instanceof RegExp ? w.test(s) : s.includes(w))));
    if (live.length) throw new Error(`${label}: SPA navigation raised page error(s):\n` + live.map((s) => `  ${s}`).join("\n"));
    console.log(`${label}: router Link SPA navigation ok (/ -> /about -> /)`);
  } finally {
    await page.close();
  }
}

// The "actually hydrates" gate (same convention as e2e/start.mjs): not just
// SSR HTML, but the client graph serving and the runtime coming alive — the
// client-only probe mounts and the counter proves handlers attached.
async function assertHydration(port, label) {
  const chromium = await loadPlaywright();
  if (!chromium) {
    console.log(`${label}: playwright not installed locally; skipped the browser hydration pass (CI runs it)`);
    return;
  }
  const base = `http://localhost:${port}`;
  const whitelist = [/favicon\.ico/];
  const browser = await chromium.launch();
  try {
    await assertHydrates(browser, `${base}/`, {
      ssrMarker: "solid-start-home-marker",
      clientMarker: '[data-testid="client-mounted"]',
      clientMarkerText: "client-mounted-ok",
      interaction: {
        click: '[data-testid="counter"]',
        expect: { selector: '[data-testid="counter"]', text: "count: 1" },
      },
      whitelist,
    });
    await assertHydrates(browser, `${base}/about`, {
      ssrMarker: "solid-start-about-marker",
      whitelist,
    });
    await assertSpaNavigation(browser, base, label, whitelist);
    console.log(`${label}: browser hydration ok (/ mounts + counter interaction, /about clean client graph)`);
  } finally {
    await browser.close();
  }
}

// Dev-server routing: static files still win; requests run concurrently
// through the runner.
async function assertDevRouting(port) {
  const pub = await get(port, "/favicon.txt");
  if (pub.status !== 200 || !pub.body.includes("solid-start-public-dir-marker") || pub.body.includes("<html")) {
    throw new Error("solid-start-dev: a publicDir file must still be served statically");
  }
  const results = await Promise.all([1, 2, 3, 4, 5, 6].map((i) => get(port, i % 2 ? "/" : "/about")));
  for (const r of results) if (r.status !== 200) throw new Error("solid-start-dev: concurrent SSR requests failed");
  console.log("solid-start-dev: static routing + concurrent SSR ok");
}

// Source edit -> rebuild -> visible in SSR: rewrite the route marker, wait
// for the dev server to serve the edit, then restore and require the original
// back (so later phases see a clean fixture). Every predicate requires HTTP
// 200: a rebuild error page showing the marker would otherwise false-pass.
async function assertEditRebuild(port) {
  const file = path.join(app, "src", "routes", "index.tsx");
  const original = fs.readFileSync(file, "utf8");
  if (!original.includes("solid-start-home-marker")) throw new Error("solid-start-dev: fixture marker missing from index.tsx");
  const edited = original.replace("solid-start-home-marker", "solid-start-home-edited-marker");
  const seesMarker = async (marker) => {
    try {
      const { status, body } = await get(port, "/");
      return status === 200 && bodyWithoutScripts(body).includes(marker);
    } catch {
      return false;
    }
  };
  const touch = () => fs.writeFileSync(file, fs.readFileSync(file, "utf8"));
  try {
    fs.writeFileSync(file, edited);
    const seen = await settles(() => seesMarker("solid-start-home-edited-marker"), {
      timeoutMs: 60000,
      pollMs: 250,
      touch,
    });
    if (!seen) throw new Error("solid-start-dev: source edit was not rebuilt into SSR output");
    console.log("solid-start-dev: source edit -> rebuild -> SSR ok");
    // The rebuilt output must still hydrate: reload the edited page through
    // the retained browser gate (same assertHydrates conventions).
    const chromium = await loadPlaywright();
    if (!chromium) {
      console.log("solid-start-dev: playwright not installed locally; skipped the browser edit/reload pass (CI runs it)");
    } else {
      const browser = await chromium.launch();
      try {
        await assertHydrates(browser, `http://localhost:${port}/`, {
          ssrMarker: "solid-start-home-edited-marker",
          clientMarker: '[data-testid="client-mounted"]',
          clientMarkerText: "client-mounted-ok",
          interaction: {
            click: '[data-testid="counter"]',
            expect: { selector: '[data-testid="counter"]', text: "count: 1" },
          },
          whitelist: [/favicon\.ico/],
        });
        console.log("solid-start-dev: edited page reload hydrates ok");
      } finally {
        await browser.close();
      }
    }
  } finally {
    fs.writeFileSync(file, original);
    const restored = await settles(() => seesMarker("solid-start-home-marker"), {
      timeoutMs: 60000,
      pollMs: 250,
      touch,
    });
    if (!restored) throw new Error("solid-start-dev: fixture restore did not rebuild back into SSR output");
  }

  // Imported-component freshness: the external TanStack router runtime caches
  // the previous router entries across reload (entriesPromise boundary), so a
  // route-file edit alone cannot prove staleness is gone. Editing the imported
  // Hero must rebuild into raw SSR too — intentionally covered against the
  // real runtime, not mocked away.
  const heroFile = path.join(app, "src", "components", "SolidStartHero.tsx");
  const heroOriginal = fs.readFileSync(heroFile, "utf8");
  if (!heroOriginal.includes("solid-start-hero-marker")) {
    throw new Error("solid-start-dev: fixture marker missing from SolidStartHero.tsx");
  }
  const heroEdited = heroOriginal.replace("solid-start-hero-marker", "solid-start-hero-edited-marker");
  const seesHero = async (present, absent) => {
    try {
      const { status, body } = await get(port, "/");
      if (status !== 200) return false;
      const raw = bodyWithoutScripts(body);
      return raw.includes(present) && !raw.includes(absent);
    } catch {
      return false;
    }
  };
  const touchHero = () => fs.writeFileSync(heroFile, fs.readFileSync(heroFile, "utf8"));
  try {
    fs.writeFileSync(heroFile, heroEdited);
    const seenHero = await settles(() => seesHero("solid-start-hero-edited-marker", "solid-start-hero-marker"), {
      timeoutMs: 60000,
      pollMs: 250,
      touch: touchHero,
    });
    if (!seenHero) throw new Error("solid-start-dev: imported component edit was not rebuilt into SSR output");
    console.log("solid-start-dev: imported component edit -> rebuild -> SSR ok");
    const heroChromium = await loadPlaywright();
    if (!heroChromium) {
      console.log("solid-start-dev: playwright not installed locally; skipped the browser imported-edit/reload pass (CI runs it)");
    } else {
      const browser = await heroChromium.launch();
      try {
        await assertHydrates(browser, `http://localhost:${port}/`, {
          ssrMarker: "solid-start-hero-edited-marker",
          clientMarker: '[data-testid="client-mounted"]',
          clientMarkerText: "client-mounted-ok",
          interaction: {
            click: '[data-testid="counter"]',
            expect: { selector: '[data-testid="counter"]', text: "count: 1" },
          },
          whitelist: [/favicon\.ico/],
        });
        console.log("solid-start-dev: edited imported component reload hydrates ok");
      } finally {
        await browser.close();
      }
    }
  } finally {
    fs.writeFileSync(heroFile, heroOriginal);
    const heroRestored = await settles(() => seesHero("solid-start-hero-marker", "solid-start-hero-edited-marker"), {
      timeoutMs: 60000,
      pollMs: 250,
      touch: touchHero,
    });
    if (!heroRestored) throw new Error("solid-start-dev: imported component restore did not rebuild back into SSR output");
  }
}

async function devPhase() {
  rm(path.join(app, ".oj-cache"));
  const reqPort = 3197;
  let log = "";
  const srv = spawn(oj, ["dev", app, "--port", String(reqPort)], { stdio: ["ignore", "pipe", "pipe"] });
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  try {
    // oj may increment off a busy port; use the port it actually bound.
    // Startup/wait failures carry the captured server log + exit/signal so a
    // dead server never reports as a bare ECONNREFUSED.
    let port = reqPort;
    const ready = await settles(
      () => {
        if (srv.exitCode != null) return true;
        const bound = parseBoundPort(log);
        if (bound) port = bound;
        return Boolean(bound);
      },
      { timeoutMs: 60000, pollMs: 250 },
    );
    const bound = parseBoundPort(log);
    if (bound) port = bound;
    if (!ready || !bound) {
      throw new Error(
        `solid-start-dev: dev server did not print its bound URL within 60s ` +
          `(exit=${srv.exitCode} signal=${srv.signalCode}); log tail:\n${log.slice(-4000)}`,
      );
    }
    if (srv.exitCode !== null) {
      throw new Error(
        `solid-start-dev: dev server exited (exit=${srv.exitCode} signal=${srv.signalCode}) ` +
          `before serving; log tail:\n${log.slice(-4000)}`,
      );
    }
    try {
      await waitUp(`http://localhost:${port}/`, { proc: srv });
    } catch (e) {
      throw new Error(
        `solid-start-dev: dev server at :${port} did not answer (${e.message}; ` +
          `exit=${srv.exitCode} signal=${srv.signalCode}); log tail:\n${log.slice(-4000)}`,
        { cause: e },
      );
    }
    await assertApp(port, "solid-start-dev");
    await assertDevRouting(port);
    await assertHydration(port, "solid-start-dev");
    await assertEditRebuild(port);
  } finally {
    // Await the shutdown before prodPhase deletes dist/.oj-cache (the
    // SIGKILLed server's node children flush into .oj-cache for a beat).
    await killAndWait(srv);
  }
}

async function prodPhase() {
  rm(path.join(app, "dist"));
  rm(path.join(app, ".oj-cache"));
  execSync(`${oj} build ${app}`, { cwd: repo, stdio: ["ignore", "ignore", "inherit"] });
  const dist = path.join(app, "dist");
  for (const f of ["server.mjs", "client"]) {
    if (!fs.existsSync(path.join(dist, f))) throw new Error(`solid-start-prod: dist/${f} missing`);
  }
  const port = 3198;
  const srv = spawn("node", [path.join(dist, "server.mjs")], {
    cwd: app,
    stdio: "ignore",
    env: { ...process.env, PORT: String(port) },
  });
  try {
    try {
      await waitUp(`http://localhost:${port}/`, { proc: srv });
    } catch (e) {
      throw new Error(
        `solid-start-prod: prod server at :${port} did not answer (${e.message}; ` +
          `exit=${srv.exitCode} signal=${srv.signalCode})`,
        { cause: e },
      );
    }
    await assertApp(port, "solid-start-prod");
    await assertHydration(port, "solid-start-prod");
  } finally {
    await killAndWait(srv);
  }
}

try {
  if (runDev) await devPhase();
  if (runProd) await prodPhase();
  console.log("\nSOLID START INTEGRATION PASSED");
} catch (e) {
  console.error("\nSOLID START INTEGRATION FAILED:", e.message);
  process.exit(1);
}
