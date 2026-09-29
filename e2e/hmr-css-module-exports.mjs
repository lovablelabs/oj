// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// A CSS module cannot self-accept (its class map changes on edit): the update
// must climb to the importing module so a class added to `a.module.css` shows
// up in `styles.bar` without a reload, as Vite's css-analysis arranges.

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
const port = Number(process.env.OJ_E2E_PORT || 5486);

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-cssmod-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "cssmod-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "a.module.css"), `.foo { color: red; }\n`);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import styles from "./a.module.css";
window.__RENDERS = (window.__RENDERS || 0) + 1;
document.getElementById("app").className = styles.foo + " " + (styles.bar || "nobar");
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

  // The served wrapper of a CSS module carries no self-accept; a plain sheet does.
  const wrapper = await (await fetch(`http://localhost:${port}/src/a.module.css?import`)).text();
  assert.ok(!wrapper.includes("import.meta.hot.accept"), "css module wrapper must not self-accept");
  fs.writeFileSync(path.join(app, "src", "plain.css"), `body { margin: 0 }\n`);
  const plain = await (await fetch(`http://localhost:${port}/src/plain.css?import`)).text();
  assert.match(plain, /import\.meta\.hot\.accept\(/, "plain css wrapper self-accepts");

  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${port}/`, { timeout: 30000 });
    await page.waitForFunction(() => window.__READY === true, { timeout: 10000 });
    const before = await page.evaluate(() => document.getElementById("app").className);
    assert.match(before, /foo/, `scoped foo class applied: ${before}`);
    assert.match(before, /nobar/, `bar not exported yet: ${before}`);
    await page.evaluate(() => {
      window.__LOADED_AT = Date.now();
    });

    fs.writeFileSync(path.join(app, "src", "a.module.css"), `.foo { color: red; }\n.bar { color: blue; }\n`);
    await page.waitForFunction(
      () =>
        !document.getElementById("app").className.includes("nobar") &&
        /bar/.test(document.getElementById("app").className),
      { timeout: 10000 },
    );
    const stillSamePage = await page.evaluate(() => typeof window.__LOADED_AT === "number");
    assert.ok(stillSamePage, "the importer was hot updated, not the page reloaded");
    const renders = await page.evaluate(() => window.__RENDERS);
    assert.equal(renders, 2, `importer re-ran exactly once: ${renders}`);
    assert.equal(errors.length, 0, `page errors: ${errors.join("|")}`);
  } finally {
    await browser.close();
  }
  console.log("HMR-CSS-MODULE-EXPORTS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("HMR-CSS-MODULE-EXPORTS E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
