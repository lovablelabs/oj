// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite's import.meta.hot.data contract: the object persists across hot swaps
// of the same module (the new instance sees what the old one stashed), so
// stateful modules can hand their state forward. Implemented in oj's HMR
// client; this is the first test that asserts it.

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
  console.log("SKIP hmr-hot-data: playwright not installed");
  process.exit(0);
}
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const PORT = 5501;

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-hot-data-"));
const cleanup = () => fs.rmSync(app, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
const w = (rel, s) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), s);
};
w("package.json", JSON.stringify({ name: "hot-data", version: "1.0.0", type: "module" }));
const store = (tag) =>
  [
    `export const tag = "${tag}";`,
    "const prev = import.meta.hot ? (import.meta.hot.data.count ?? 0) : 0;",
    "window.__counts = (window.__counts || []).concat(prev);",
    "if (import.meta.hot) {",
    "  import.meta.hot.data.count = prev + 1;",
    "  import.meta.hot.accept();",
    "}",
    "",
  ].join("\n");
w("src/store.js", store("v1"));
w("src/main.js", 'import "./store.js";\nwindow.__READY = true;\n');
w("index.html", `<!doctype html><html><body><script type="module" src="/src/main.js"></script></body></html>`);

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
  const counts = () => page.evaluate(() => window.__counts);

  // First execution stashes count=1 after seeing 0.
  if (JSON.stringify(await counts()) !== "[0]") throw new Error(`first run saw ${JSON.stringify(await counts())}`);
  w("src/store.js", store("v2"));
  await page.waitForFunction(() => window.__counts && window.__counts.length === 2, null, { timeout: 20000 });
  w("src/store.js", store("v3"));
  await page.waitForFunction(() => window.__counts && window.__counts.length === 3, null, { timeout: 20000 });

  const seen = await counts();
  if (JSON.stringify(seen) !== "[0,1,2]") {
    throw new Error(`hot.data did not persist across swaps: each new instance saw ${JSON.stringify(seen)}, want [0,1,2]`);
  }
  const reloaded = await page.evaluate(() => !window.__NO_RELOAD);
  if (reloaded) throw new Error("a self-accepting module edit reloaded the page");
  console.log("hmr-hot-data: import.meta.hot.data persists across swaps ([0,1,2]), no reload");
} catch (e) {
  failed = true;
  console.error("HMR HOT DATA FAILED:", e.message ?? e);
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
