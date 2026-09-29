// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Generic `import.meta.hot` in a plain (non-React) app, as Vite defines it:
//  - `import.meta.hot.accept()` makes a module self-accepting;
//  - `import.meta.hot.accept('./dep', cb)` makes the importer the boundary for
//    that dependency and hands the callback the new dependency module;
//  - `import.meta.hot.accept([deps], cb)` receives an array aligned with deps;
//  - `import.meta.hot.dispose(cb)` runs before the module is replaced.
// None of these reload the page.

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
const PORT = 5493;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-hotaccept-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
const w = (rel, s) => fs.writeFileSync(path.join(app, rel), s);
w("package.json", JSON.stringify({ name: "hot-accept", version: "1.0.0" }));
w("src/util.js", `export const value = "u1";\n`);
w("src/other.js", `export const other = "o1";\n`);
w(
  "src/store.js",
  `window.__storeRuns = (window.__storeRuns || 0) + 1;\nexport const s = "s1";\n` +
    `if (import.meta.hot) {\n  import.meta.hot.dispose(() => { window.__storeDisposed = (window.__storeDisposed || 0) + 1; });\n` +
    `  import.meta.hot.accept((m) => { window.__storeAccepted = m.s; });\n}\n`,
);
w(
  "src/main.js",
  `import { value } from "./util.js";\nimport { other } from "./other.js";\nimport "./store.js";\n` +
    `window.__util = value; window.__other = other; window.__READY = true;\n` +
    `if (import.meta.hot) {\n  import.meta.hot.accept("./util.js", (m) => { window.__util = m.value; });\n` +
    `  import.meta.hot.accept(["./other.js", "./util.js"], ([o, u]) => { window.__arr = [o && o.other, u && u.value]; });\n}\n`,
);
w(
  "index.html",
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${PORT}/`);
  const main = await (await fetch(`http://localhost:${PORT}/src/main.js`)).text();
  assert.match(
    main,
    /createHotContext\("\/src\/main\.js"\)/,
    `hot context injected for a module using import.meta.hot:\n${main}`,
  );
  assert.match(main, /accept\("\/src\/util\.js"/, "accept dep specifier rewritten to its served url");
  const util = await (await fetch(`http://localhost:${PORT}/src/util.js`)).text();
  assert.doesNotMatch(util, /createHotContext/, "a module not using import.meta.hot gets no context");

  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${PORT}/`, { timeout: 30000 });
    await page.waitForFunction(() => window.__READY === true, { timeout: 20000 });
    await page.evaluate(() => {
      window.__NOT_RELOADED = true;
    });

    // dep accept: editing util updates main's binding through the callback
    w("src/util.js", `export const value = "u2";\n`);
    await page.waitForFunction(() => window.__util === "u2", { timeout: 20000 });
    const arr = await page.evaluate(() => window.__arr);
    assert.deepEqual(arr, [undefined, "u2"], "array form gets the new module in its slot only");

    // array-form dep: editing other
    w("src/other.js", `export const other = "o2";\n`);
    await page.waitForFunction(() => Array.isArray(window.__arr) && window.__arr[0] === "o2", { timeout: 20000 });

    // self accept: store is re-imported, dispose ran, callback got new exports
    w(
      "src/store.js",
      `window.__storeRuns = (window.__storeRuns || 0) + 1;\nexport const s = "s2";\n` +
        `if (import.meta.hot) {\n  import.meta.hot.dispose(() => { window.__storeDisposed = (window.__storeDisposed || 0) + 1; });\n` +
        `  import.meta.hot.accept((m) => { window.__storeAccepted = m.s; });\n}\n`,
    );
    await page.waitForFunction(() => window.__storeAccepted === "s2", { timeout: 20000 });
    const st = await page.evaluate(() => ({
      runs: window.__storeRuns,
      disposed: window.__storeDisposed,
      reloaded: window.__NOT_RELOADED !== true,
    }));
    assert.equal(st.runs, 2, "store executed exactly twice");
    assert.equal(st.disposed, 1, "dispose ran once before replacement");
    assert.equal(st.reloaded, false, "page never reloaded");
    assert.equal(errors.length, 0, `page errors: ${errors.join("|")}`);
    console.log("HMR-HOT-ACCEPT E2E PASSED");
  } finally {
    await browser.close();
  }
} catch (err) {
  failed = true;
  console.error("HMR-HOT-ACCEPT E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
