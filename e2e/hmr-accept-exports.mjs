// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite's partial accept (import.meta.hot.acceptExports): a module accepting
// updates for a LISTED export is a boundary when its importers only use the
// listed exports (hot swap, callback fires, no reload); an importer using an
// UNLISTED export must still propagate past it (full reload here, since
// nothing above accepts). oj parses acceptExports and has graph-level tests;
// this is the first behavior test.

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
  console.log("SKIP hmr-accept-exports: playwright not installed");
  process.exit(0);
}
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const PORT = 5502;

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-accept-exports-"));
const cleanup = () => fs.rmSync(app, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
const w = (rel, s) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), s);
};
w("package.json", JSON.stringify({ name: "accept-exports", version: "1.0.0", type: "module" }));
const mod = (av, bv) =>
  [
    `export const a = "${av}";`,
    `export const b = "${bv}";`,
    "if (import.meta.hot) {",
    '  import.meta.hot.acceptExports(["a"], (m) => { window.__a = m.a; });',
    "}",
    "",
  ].join("\n");
w("src/mod.js", mod("a1", "b1"));
w("src/main.js", 'import { a } from "./mod.js";\nwindow.__a = a;\nwindow.__READY = true;\n');
w("index.html", `<!doctype html><html><body><script type="module" src="/src/main.js"></script></body></html>`);
// A second page whose importer uses the UNLISTED export b.
w("src/main-b.js", 'import { b } from "./mod-b.js";\nwindow.__b = b;\nwindow.__READY = true;\n');
w("src/mod-b.js", mod("a1", "b1").replace("__a = m.a", "__a = m.a"));
w("other.html", `<!doctype html><html><body><script type="module" src="/src/main-b.js"></script></body></html>`);

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

  // Scenario 1: the importer uses only the ACCEPTED export -> hot swap.
  const page = await browser.newPage();
  await page.goto(`http://localhost:${PORT}/`);
  await page.waitForFunction(() => window.__READY, null, { timeout: 20000 });
  await page.evaluate(() => (window.__NO_RELOAD = true));
  w("src/mod.js", mod("a2", "b1"));
  await page.waitForFunction(() => window.__a === "a2", null, { timeout: 20000 });
  const reloaded = await page.evaluate(() => !window.__NO_RELOAD);
  if (reloaded) throw new Error("an accepted-export edit reloaded the page instead of hot-swapping");
  console.log("hmr-accept-exports: accepted-export edit hot-swaps via the callback, no reload");

  // Scenario 2: the importer uses the UNLISTED export -> the update must
  // propagate past the partial boundary (a full reload, nothing else accepts).
  const page2 = await browser.newPage();
  await page2.goto(`http://localhost:${PORT}/other.html`);
  await page2.waitForFunction(() => window.__READY, null, { timeout: 20000 });
  await page2.evaluate(() => (window.__NO_RELOAD = true));
  w("src/mod-b.js", mod("a1", "b2"));
  // Either the page reloads (marker gone) or the importer re-ran with b2.
  try {
    await page2.waitForFunction(() => !window.__NO_RELOAD || window.__b === "b2", null, { timeout: 20000 });
  } catch {
    throw new Error(
      "VITE GAP: an edit reaching an UNLISTED export was swallowed by the acceptExports boundary; " +
        "Vite propagates it past the partial boundary (here: full reload). " +
        "oj must compare the importers' used names against the accepted list.",
    );
  }
  const b = await page2.evaluate(() => window.__b);
  if (b !== "b2") throw new Error(`the unlisted export never reached the importer: window.__b = ${JSON.stringify(b)}`);
  console.log("hmr-accept-exports: unlisted-export edit propagates past the partial boundary");
} catch (e) {
  failed = true;
  console.error("HMR ACCEPT EXPORTS FAILED:", e.message ?? e);
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
