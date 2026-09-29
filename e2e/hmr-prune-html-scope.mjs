// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Two Vite client behaviors:
//  1. prune: deleting an `import "./a.css"` line removes the injected <style>
//     (the server sends {type:"prune"} once the importer drops the edge, and
//     the css wrapper registered hot.prune(removeStyle)); re-adding the import
//     brings the stylesheet back without a reload.
//  2. an edited .html reloads only the tabs showing that page (the full-reload
//     frame names it in `path`); index.html reloads every page.

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
const port = Number(process.env.OJ_E2E_PORT || 5487);

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-prune-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "prune-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "a.css"), `body { background-color: rgb(1, 2, 3); }\n`);
const withCss = `import "./a.css";\nwindow.__READY = true;\nif (import.meta.hot) import.meta.hot.accept();\n`;
const withoutCss = `window.__READY = true;\nif (import.meta.hot) import.meta.hot.accept();\n`;
fs.writeFileSync(path.join(app, "src", "main.js"), withCss);
fs.writeFileSync(path.join(app, "src", "about.js"), `window.__READY = true;\n`);
const html = (title, script) =>
  `<!doctype html><html><head><title>${title}</title></head><body><h1>${title}</h1><script type="module" src="${script}"></script></body></html>`;
fs.writeFileSync(path.join(app, "index.html"), html("home", "/src/main.js"));
fs.writeFileSync(path.join(app, "about.html"), html("about", "/src/about.js"));

const bg = (page) => page.evaluate(() => getComputedStyle(document.body).backgroundColor);

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${port}/`);

  const browser = await chromium.launch();
  const home = await browser.newPage();
  const about = await browser.newPage();
  const errors = [];
  home.on("pageerror", (e) => errors.push(String(e)));
  about.on("pageerror", (e) => errors.push(String(e)));
  try {
    await home.goto(`http://localhost:${port}/`, { timeout: 30000 });
    await home.waitForFunction(() => window.__READY === true, { timeout: 10000 });
    await about.goto(`http://localhost:${port}/about.html`, { timeout: 30000 });
    await about.waitForFunction(() => window.__READY === true, { timeout: 10000 });
    assert.equal(await bg(home), "rgb(1, 2, 3)", "stylesheet applied on load");
    await home.evaluate(() => { window.__MARK = 1; });

    // 1. prune: drop the css import; the <style> goes away, no reload.
    fs.writeFileSync(path.join(app, "src", "main.js"), withoutCss);
    await home.waitForFunction(() => getComputedStyle(document.body).backgroundColor === "rgba(0, 0, 0, 0)", { timeout: 10000 });
    assert.equal(await home.evaluate(() => window.__MARK), 1, "prune did not reload the page");
    assert.equal(await home.evaluate(() => document.querySelectorAll("style[data-oj-id]").length), 0, "style tag removed");

    // Re-adding the import re-runs the (stamped) stylesheet module.
    fs.writeFileSync(path.join(app, "src", "main.js"), withCss);
    await home.waitForFunction(() => getComputedStyle(document.body).backgroundColor === "rgb(1, 2, 3)", { timeout: 10000 });
    assert.equal(await home.evaluate(() => window.__MARK), 1, "re-import did not reload the page");

    // 2. html reload scope: editing about.html reloads only the about tab.
    await about.evaluate(() => { window.__MARK = 1; });
    fs.writeFileSync(path.join(app, "about.html"), html("about v2", "/src/about.js"));
    await about.waitForFunction(() => document.title === "about v2", { timeout: 10000 });
    await sleep(500);
    assert.equal(await home.evaluate(() => window.__MARK), 1, "home tab kept its state when about.html changed");

    // index.html is the shell behind every route: every tab reloads.
    fs.writeFileSync(path.join(app, "index.html"), html("home v2", "/src/main.js"));
    await home.waitForFunction(() => document.title === "home v2", { timeout: 10000 });
    assert.equal(errors.length, 0, `page errors: ${errors.join("|")}`);
  } finally {
    await browser.close();
  }
  console.log("HMR-PRUNE-HTML-SCOPE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("HMR-PRUNE-HTML-SCOPE E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
