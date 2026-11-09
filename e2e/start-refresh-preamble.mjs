// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// TanStack Start dev with hot updates (plain, and behind the editor's granular
// HMR gate as a hosted dev server runs it): a fresh page hydrates, and React
// Fast Refresh is wired. The dev client bundle is code-split, and the refresh
// preamble used to be a module inlined in the entry chunk's body, after the
// shared chunks the entry imports: a component the root and a lazy route share
// ran its `$RefreshReg$` check first and threw "@vitejs/plugin-react can't
// detect preamble" on every fresh load, so the page never hydrated. The
// preamble is the refresh runtime module's own tail now, evaluated before any
// module that imports it, wherever rolldown chunks the graph.
//
// Three checks per boot: the page hydrates with no page error (a click reaches
// a React handler), the preamble ran before react-dom (the devtools hook it
// installs saw the renderer), and the served chunks keep the order invariant
// (every chunk with a wrapped module imports the runtime's chunk, or is it and
// renders the preamble ahead of its first check). The second boot restores the
// bundle from the cache, which is keyed by hot mode: a bundle built without
// hot updates is never served to a hot session.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { rmrf, settles, sleep, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "..");
const app = path.join(repo, "e2e", "fixtures", "start-app");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 5411);
const base = `http://localhost:${PORT}`;
const cache = path.join(app, ".oj-cache");
const chunksDir = path.join(cache, "v1", "start", "client-chunks");

if (!fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start"))) {
  console.log("SKIP start refresh preamble: fixture deps not installed");
  process.exit(0);
}
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

const CHECK = "can't detect preamble";
// The component the Fast Refresh round trip edits (as start-hot-update.mjs).
const widget = path.join(app, "src", "widgets", "glob-widget.jsx");
const widgetSource = fs.readFileSync(widget, "utf8");
process.on("exit", () => fs.writeFileSync(widget, widgetSource));
const STUBS = "window.$RefreshReg$ = () => {};";

// The order invariant over the chunks on disk (what the pages are served).
function checkChunkOrder(refresh) {
  const files = fs.readdirSync(chunksDir).filter((f) => f.endsWith(".js"));
  const codes = new Map(files.map((f) => [f, fs.readFileSync(path.join(chunksDir, f), "utf8")]));
  const withStubs = [...codes].filter(([, code]) => code.includes(STUBS)).map(([f]) => f);
  if (!refresh) {
    // A full-reload gate bundles without the refresh plugins: no preamble, no wrapper.
    must(withStubs.length === 0, `no preamble without hot updates, got ${JSON.stringify(withStubs)}`);
    const wrapped = [...codes].filter(([, code]) => code.includes(CHECK)).map(([f]) => f);
    must(wrapped.length === 0, `no wrapped module without hot updates, got ${JSON.stringify(wrapped)}`);
    return { files: files.length, wrapped: 0, runtimeChunk: null };
  }
  must(withStubs.length === 1, `one chunk installs the preamble, got ${JSON.stringify(withStubs)}`);
  const runtimeChunk = withStubs[0];
  must(
    codes.get(runtimeChunk).includes("function injectIntoGlobalHook"),
    `the preamble is the runtime module's tail (${runtimeChunk})`,
  );
  let wrapped = 0;
  for (const [name, code] of codes) {
    const check = code.indexOf(CHECK);
    if (check === -1) continue;
    wrapped++;
    if (name === runtimeChunk) {
      must(code.indexOf(STUBS) < check, `${name}: the preamble precedes its first wrapper check`);
    } else {
      must(code.includes(`"./${runtimeChunk}"`), `${name} imports the runtime's chunk ${runtimeChunk}`);
    }
  }
  return { files: files.length, wrapped, runtimeChunk };
}

async function boot(env, label, { refresh = true, roundTrip = false } = {}) {
  let log = "";
  const srv = spawn(oj, ["dev", app, "--port", String(PORT)], {
    detached: true,
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, ...env },
  });
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  const exited = new Promise((r) => srv.once("exit", r));
  const stop = async () => {
    try {
      process.kill(-srv.pid, "SIGTERM");
    } catch {}
    await Promise.race([exited, sleep(5000)]);
    try {
      process.kill(-srv.pid, "SIGKILL");
    } catch {}
  };
  try {
    await waitUp(`${base}/`, { proc: srv, timeoutMs: 240000 });
    await settles(() => fs.existsSync(path.join(chunksDir)) && fs.readdirSync(chunksDir).length > 0);
    const order = checkChunkOrder(refresh);
    if (refresh) must(order.wrapped > 0, "the bundle has wrapped component modules (Fast Refresh is on)");
    console.log(
      `${label}: ${order.files} chunk(s), ${order.wrapped} with wrapped modules, preamble in ${order.runtimeChunk}`,
    );

    const browser = await chromium.launch();
    try {
      const page = await browser.newPage();
      const errors = [];
      page.on("pageerror", (e) => errors.push(e.message));
      page.on("console", (m) => {
        if (m.type() === "error") errors.push(m.text());
      });
      await page.goto(`${base}/`, { waitUntil: "networkidle" });
      // Hydration: the client entry ran to the end and React took the page
      // (react-dom tells the devtools hook, if any, about its root).
      await settles(async () => (await page.getByTestId("counter").count()) > 0, { timeoutMs: 30000 });
      await sleep(500);
      const state = await page.evaluate(() => ({
        refreshReg: typeof window.$RefreshReg$,
        refreshSig: typeof window.$RefreshSig$,
        hook: typeof window.__REACT_DEVTOOLS_GLOBAL_HOOK__,
        supportsFiber: window.__REACT_DEVTOOLS_GLOBAL_HOOK__?.supportsFiber ?? null,
      }));
      must(!errors.some((e) => e.includes(CHECK)), `no preamble error: ${JSON.stringify(errors)}`);
      must(errors.length === 0, `no page errors: ${JSON.stringify(errors)}`);
      if (refresh) {
        must(
          state.refreshReg === "function" && state.refreshSig === "function",
          `preamble stubs installed: ${JSON.stringify(state)}`,
        );
        // The devtools hook is the preamble's (react-refresh installs one
        // when the page has none).
        must(
          state.hook === "object" && state.supportsFiber === true,
          `the preamble installed the refresh hook: ${JSON.stringify(state)}`,
        );
      } else {
        must(state.refreshReg === "undefined", `no preamble without hot updates: ${JSON.stringify(state)}`);
      }
      // Interactivity: a React handler answers a click (the fixture's index
      // route counts clicks on its button).
      const counter = page.getByTestId("counter");
      await counter.click();
      await settles(async () => (await counter.textContent()) === "count: 1", { timeoutMs: 10000 });
      if (roundTrip) {
        // Fast Refresh itself: the preamble's injectIntoGlobalHook ran before
        // react-dom evaluated, so the runtime knows the renderer and a
        // component edit re-renders in place, state and page kept.
        await page.evaluate(() => (window.__refreshProbe = "kept"));
        fs.writeFileSync(widget, widgetSource.replace('"jsxglob:"', '"jsxhot:"'));
        await page.waitForSelector('[data-testid="glob-jsx"]:has-text("jsxhot:")', { timeout: 30000 });
        must((await counter.textContent()) === "count: 1", "the hot update reset component state");
        must((await page.evaluate(() => window.__refreshProbe)) === "kept", "the hot update reloaded the page");
        fs.writeFileSync(widget, widgetSource);
        await page.waitForSelector('[data-testid="glob-jsx"]:has-text("jsxglob:")', { timeout: 30000 });
      }
      console.log(`${label}: hydrated, ${refresh ? "refresh hook installed" : "no refresh"}, no page errors`);
    } finally {
      await browser.close();
    }
  } catch (e) {
    console.error(
      log
        .split("\n")
        .filter((l) => !l.includes("plugin(s) active"))
        .join("\n"),
    );
    throw e;
  } finally {
    await stop();
  }
}

await rmrf(cache);
// Plain hot dev: a fresh cache builds the bundle in the hot engine.
await boot({}, "hot, fresh cache", { roundTrip: true });
// A hosted dev server's granular gate (hot updates parked for the editor's
// flush): the same hot bundle, restored from the cache.
await boot({ OJ_HMR_GATE: "1", OJ_HMR_FULL_RELOAD: "false" }, "granular gate, cached bundle");
// The full-reload gate builds without the refresh plugins. Its bundle is
// keyed apart: the next hot boot must not restore it.
await boot({ OJ_HMR_GATE: "1", OJ_HMR_FULL_RELOAD: "true" }, "full-reload gate", { refresh: false });
await boot({ OJ_HMR_GATE: "1", OJ_HMR_FULL_RELOAD: "false" }, "granular gate after a full-reload build");
console.log("PASS start refresh preamble");
