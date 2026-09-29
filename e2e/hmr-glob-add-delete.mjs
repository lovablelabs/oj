// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// import.meta.glob reacts to files appearing and disappearing: creating
// `pages/b.js` under `import.meta.glob("./pages/*.js")` hot updates the
// importer with the new key, deleting it removes the key, with no page reload
// (Vite's importMetaGlob hotUpdate on create/delete).

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const port = Number(process.env.OJ_E2E_PORT || 5491);

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-globhmr-"));
fs.mkdirSync(path.join(app, "src", "pages"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "glob-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "pages", "a.js"), `export default "A";\n`);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `const pages = import.meta.glob("./pages/*.js", { eager: true });
window.__KEYS = Object.keys(pages).sort();
document.getElementById("app").textContent = window.__KEYS.join(",");
window.__READY = true;
if (import.meta.hot) import.meta.hot.accept();
`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><div id="app"></div><script type="module" src="/src/main.js"></script></body></html>`,
);

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${port}/`);
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${port}/`, { timeout: 30000 });
    await page.waitForFunction(() => window.__READY === true, { timeout: 10000 });
    assert.deepEqual(await page.evaluate(() => window.__KEYS), ["./pages/a.js"]);
    await page.evaluate(() => { window.__MARK = 1; });

    // create: the new file shows up in the glob without a reload
    fs.writeFileSync(path.join(app, "src", "pages", "b.js"), `export default "B";\n`);
    await page.waitForFunction(() => document.getElementById("app").textContent === "./pages/a.js,./pages/b.js", { timeout: 10000 });
    assert.equal(await page.evaluate(() => window.__MARK), 1, "create did not reload the page");

    // delete: the key goes away, still no reload
    fs.unlinkSync(path.join(app, "src", "pages", "a.js"));
    await page.waitForFunction(() => document.getElementById("app").textContent === "./pages/b.js", { timeout: 10000 });
    assert.equal(await page.evaluate(() => window.__MARK), 1, "delete did not reload the page");
    assert.equal(errors.length, 0, `page errors: ${errors.join("|")}`);
  } finally {
    await browser.close();
  }
  console.log("HMR-GLOB-ADD-DELETE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("HMR-GLOB-ADD-DELETE E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
