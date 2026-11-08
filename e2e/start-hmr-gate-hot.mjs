// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Start dev behind the editor's granular HMR gate (OJ_HMR_GATE=1,
// OJ_HMR_FULL_RELOAD=false): an edit still hot-updates in place instead of
// reloading the page. The patch is built at once but parked until the
// editor's POST /__hmr_flush releases it.
//
// The loop replays an editor's edit: write the file, poll GET /__hmr_gate
// until it holds the path, then flush. A gated server used to turn hot
// updates off, so every such edit reloaded the page. The full-reload gate's
// page reload is start-hmr-gate.mjs.

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
const PORT = Number(process.env.OJ_E2E_PORT || 5406);
const base = `http://localhost:${PORT}`;

if (!fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start"))) {
  console.log("SKIP start hmr gate hot: fixture deps not installed");
  process.exit(0);
}
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

const widget = path.join(app, "src", "widgets", "glob-widget.jsx");
const original = fs.readFileSync(widget, "utf8");
const withText = (text) => original.replace('"jsxglob:"', JSON.stringify(text));
process.on("exit", () => fs.writeFileSync(widget, original));

// The edited path in the gate's pending list, polled every 50ms for 2s.
const needle = `/src/widgets/glob-widget.jsx":`;
async function gateHolds() {
  const until = Date.now() + 2000;
  while (Date.now() < until) {
    const body = await (await fetch(`${base}/__hmr_gate`)).text().catch(() => "");
    if (body.includes(needle)) return true;
    await sleep(50);
  }
  return false;
}
const flush = async () => (await fetch(`${base}/__hmr_flush`, { method: "POST" })).json();

await rmrf(path.join(app, ".oj-cache"));
let log = "";
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], {
  detached: true,
  stdio: ["ignore", "pipe", "pipe"],
  env: { ...process.env, OJ_HMR_GATE: "1", OJ_HMR_FULL_RELOAD: "false" },
});
srv.stdout.on("data", (d) => (log += d));
srv.stderr.on("data", (d) => (log += d));
const exited = new Promise((r) => srv.once("exit", r));
const stop = async () => {
  try {
    process.kill(-srv.pid, "SIGTERM");
  } catch {}
  const kill = setTimeout(() => {
    try {
      process.kill(-srv.pid, "SIGKILL");
    } catch {}
  }, 3000);
  await exited;
  clearTimeout(kill);
};

const browser = await chromium.launch();
let failed = false;
try {
  await waitUp(`${base}/`, { proc: srv });
  must(log.includes("hmr gate: on (granular)"), `the gate did not come up granular:\n${log}`);
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  let navigations = 0;
  page.on("framenavigated", (f) => {
    if (f === page.mainFrame()) navigations++;
  });
  await page.goto(`${base}/`);
  await page.waitForSelector('[data-testid="client-mounted"]', { timeout: 60000 });
  await page.click('[data-testid="counter"]');
  await page.waitForSelector('[data-testid="counter"]:has-text("count: 1")');
  await page.evaluate(() => (window.__hotProbe = "kept"));
  const loaded = navigations;
  const shows = (text) =>
    page
      .locator('[data-testid="glob-jsx"]')
      .textContent()
      .then((t) => t?.includes(text));

  // 1. A held edit: hot-updated at once, shown only after the flush.
  const hotCount = () => log.split("oj start: hot updated").length - 1;
  const before = hotCount();
  const mark = log.length;
  fs.writeFileSync(widget, withText("jsxheld:"));
  must(await gateHolds(), "the gate never listed the edited file as pending");
  must(
    await settles(() => hotCount() > before || /oj start: (rebuilt|updated)/.test(log.slice(mark)), {
      timeoutMs: 30000,
      pollMs: 50,
    }),
    "the dev server did not finish the batch",
  );
  must(hotCount() > before, `the gated edit was rebuilt for a reload instead of hot-updated:\n${log.slice(mark)}`);
  await sleep(500);
  must(!(await shows("jsxheld:")), "the hot update reached the page before the editor's flush");
  const flushed = await flush();
  must(flushed.count >= 1, `the flush released nothing: ${JSON.stringify(flushed)}`);
  await page.waitForSelector('[data-testid="glob-jsx"]:has-text("jsxheld:")', { timeout: 15000 });
  must(navigations === loaded, "the released edit reloaded the page instead of applying in place");
  must((await page.evaluate(() => window.__hotProbe)) === "kept", "the released edit reloaded the page");
  must((await page.textContent('[data-testid="counter"]')) === "count: 1", "the released edit reset component state");
  console.log("held edit: hot-updated, parked until the flush, applied in place");

  // 2. Back-to-back edits, as the editor sends them.
  for (let i = 1; i <= 10; i++) {
    const text = `jsxquick${i}:`;
    const t0 = Date.now();
    fs.writeFileSync(widget, withText(text));
    must(await gateHolds(), `edit ${i}: the gate never held the write`);
    await flush();
    await page.waitForSelector(`[data-testid="glob-jsx"]:has-text("${text}")`, { timeout: 15000 });
    must(navigations === loaded, `edit ${i} reloaded the page`);
    console.log(`edit ${i}: in place in ${Date.now() - t0}ms`);
  }
  must((await page.evaluate(() => window.__hotProbe)) === "kept", "an edit reloaded the page");
  must((await page.textContent('[data-testid="counter"]')) === "count: 1", "an edit reset component state");
  const status = await (await fetch(`${base}/__hmr_gate`)).json();
  must(status.count === 0 && !status.heldReload, `the gate kept something held: ${JSON.stringify(status)}`);
  must(errors.length === 0, `page errors:\n${errors.join("\n")}`);
  console.log("ok: granular gate keeps Start edits hot, released by the editor's flush");
} catch (e) {
  failed = true;
  console.error(String(e?.stack ?? e));
  console.error(`--- oj log (tail)\n${log.slice(-3000)}`);
} finally {
  await browser.close();
  await stop();
}
process.exit(failed ? 1 : 0);
