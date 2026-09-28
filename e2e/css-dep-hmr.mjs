// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// CSS dependency HMR, Vite's backend-integration/css contract: editing a file
// a stylesheet pulls in (an @import-ed css file, a sass partial) recompiles
// the IMPORTING stylesheet and hot-swaps it without a page reload. oj inlines
// @imports and resolves sass uses at compile time; this asserts the watch
// edge from the dependency back to the importer actually exists.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
let chromium;
try {
  ({ chromium } = createRequire(path.join(here, "x.js"))("playwright"));
} catch {
  console.log("SKIP css-dep-hmr: playwright not installed");
  process.exit(0);
}
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const PORT = 5499;

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-css-dep-hmr-"));
const cleanup = () => fs.rmSync(app, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
const w = (rel, s) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), s);
};
w("package.json", JSON.stringify({ name: "css-dep-hmr", version: "1.0.0", type: "module" }));
w("src/part.css", ".box { color: rgb(10, 20, 30); }\n");
w("src/main.css", '@import "./part.css";\n.other { padding: 1px; }\n');
w("src/_dep.scss", "$boxcolor: rgb(40, 50, 60);\n");
w("src/styles.scss", '@use "./dep" as d;\n.sbox { color: d.$boxcolor; }\n');
w("src/main.js", 'import "./main.css";\nimport "./styles.scss";\nwindow.__READY = true;\n');
w(
  "index.html",
  `<!doctype html><html><head></head><body><div class="box">a</div><div class="sbox">b</div><script type="module" src="/src/main.js"></script></body></html>`,
);

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore", detached: true });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let browser;
try {
  for (let i = 0; i < 100; i++) {
    try {
      if ((await fetch(`http://localhost:${PORT}/`)).ok) break;
    } catch {}
    await sleep(200);
  }
  browser = await chromium.launch();
  const page = await browser.newPage();
  await page.goto(`http://localhost:${PORT}/`);
  await page.waitForFunction(() => window.__READY, null, { timeout: 20000 });
  await page.evaluate(() => (window.__NO_RELOAD = true));
  const color = (sel) => page.evaluate((s) => getComputedStyle(document.querySelector(s)).color, sel);
  const waitColor = async (sel, want, label) => {
    for (let i = 0; i < 80; i++) {
      if ((await color(sel)) === want) return;
      await sleep(250);
    }
    throw new Error(`${label}: wanted ${want}, still ${await color(sel)}`);
  };
  if ((await color(".box")) !== "rgb(10, 20, 30)") throw new Error(`initial @import color wrong: ${await color(".box")}`);
  if ((await color(".sbox")) !== "rgb(40, 50, 60)") throw new Error(`initial sass color wrong: ${await color(".sbox")}`);

  // Edit the @import-ED file: the importing stylesheet must recompile + swap.
  w("src/part.css", ".box { color: rgb(11, 22, 33); }\n");
  await waitColor(".box", "rgb(11, 22, 33)", "@import dependency edit did not reach the page");

  // Edit the sass PARTIAL: same contract through the preprocessor graph.
  w("src/_dep.scss", "$boxcolor: rgb(44, 55, 66);\n");
  await waitColor(".sbox", "rgb(44, 55, 66)", "sass partial edit did not reach the page");

  const reloaded = await page.evaluate(() => !window.__NO_RELOAD);
  if (reloaded) throw new Error("a css dependency edit full-reloaded the page instead of hot-swapping");
  console.log("css-dep-hmr: @import and sass-partial edits hot-swap the importing stylesheet, no reload");
} catch (e) {
  failed = true;
  console.error("CSS DEP HMR FAILED:", e.message ?? e);
} finally {
  if (browser) await browser.close().catch(() => {});
  try {
    process.kill(-srv.pid, "SIGKILL");
  } catch {
    try {
      srv.kill("SIGKILL");
    } catch {}
  }
  cleanup();
}
process.exit(failed ? 1 : 0);
