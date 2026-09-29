// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// build.cssCodeSplit (Vite default): each chunk gets its own stylesheet. The
// entry + statically-imported CSS is linked render-blocking in index.html; a
// dynamically-imported chunk's CSS lives in a separate file that self-injects
// when the chunk loads, so it is absent from the HTML until the import runs.

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

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-csssplit-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "css-split-app", version: "1.0.0" }));

fs.writeFileSync(path.join(app, "src", "entry.css"), `body { background: rgb(1, 2, 3); }\n`);
fs.writeFileSync(path.join(app, "src", "comp.css"), `.comp { color: rgb(4, 5, 6); }\n`);
fs.writeFileSync(path.join(app, "src", "lazy.css"), `.lazy { color: rgb(7, 8, 9); }\n`);

fs.writeFileSync(
  path.join(app, "src", "comp.js"),
  `import "./comp.css";\nexport function mountComp() { const d = document.createElement("div"); d.className = "comp"; d.textContent = "comp"; document.body.appendChild(d); }\n`,
);
fs.writeFileSync(
  path.join(app, "src", "lazy.js"),
  `import "./lazy.css";\nexport function mountLazy() { const d = document.createElement("div"); d.className = "lazy"; d.textContent = "lazy"; document.body.appendChild(d); }\n`,
);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import "./entry.css";\nimport { mountComp } from "./comp.js";\nmountComp();\nwindow.__loadLazy = () => import("./lazy.js").then((m) => m.mountLazy());\nwindow.__ready = true;\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

let failed = false;
try {
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const assetsDir = path.join(app, "dist", "assets");
  const assets = fs.readdirSync(assetsDir);
  const cssFiles = assets.filter((f) => f.endsWith(".css"));
  assert.ok(cssFiles.length >= 2, `expected split CSS (>=2 files), got: ${cssFiles.join(",")}`);

  // lightningcss minifies colors, so match the hex forms (rgb(1,2,3) -> #010203).
  const readCss = (f) => fs.readFileSync(path.join(assetsDir, f), "utf8");
  const entryCss = cssFiles.filter((f) => /#010203/i.test(readCss(f)));
  const lazyCss = cssFiles.filter((f) => /#070809/i.test(readCss(f)));
  assert.equal(entryCss.length, 1, `entry style should be in exactly one chunk css: ${entryCss.join(",")}`);
  assert.equal(lazyCss.length, 1, `lazy style should be in exactly one chunk css: ${lazyCss.join(",")}`);
  assert.notEqual(entryCss[0], lazyCss[0], "lazy CSS must be a separate file from the entry CSS");
  // comp.js is statically imported by main -> its CSS merges into the entry chunk's stylesheet.
  assert.match(readCss(entryCss[0]), /#040506/i, "statically-imported comp CSS must merge into the entry stylesheet");

  const html = fs.readFileSync(path.join(app, "dist", "index.html"), "utf8");
  assert.match(html, new RegExp(entryCss[0].replace(/[.]/g, "\\$&")), "entry CSS must be linked render-blocking in HTML");
  assert.ok(!html.includes(lazyCss[0]), "lazy CSS must NOT be linked in the HTML (loads on demand)");

  // The async chunk carries the self-injector referencing its own stylesheet.
  const lazyJs = assets.find((f) => f.startsWith("lazy-") && f.endsWith(".js"));
  assert.ok(lazyJs, `no lazy chunk emitted: ${assets.join(",")}`);
  assert.match(fs.readFileSync(path.join(assetsDir, lazyJs), "utf8"), /createElement\("link"\)|createElement\('link'\)/, "lazy chunk must self-inject its stylesheet");

  const srv = spawn(oj, ["preview", app, "--port", "5388"], { stdio: "ignore" });
  await waitUp("http://localhost:5388/");
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto("http://localhost:5388/", { timeout: 30000 });
    await page.waitForFunction(() => window.__ready, { timeout: 10000 });
    // Entry + statically-imported styles applied on first paint.
    const bg = await page.evaluate(() => getComputedStyle(document.body).backgroundColor);
    assert.equal(bg, "rgb(1, 2, 3)", "entry stylesheet not applied");
    const compColor = await page.evaluate(() => getComputedStyle(document.querySelector(".comp")).color);
    assert.equal(compColor, "rgb(4, 5, 6)", "statically-imported stylesheet not applied");
    // Lazy stylesheet is not present until the dynamic import runs.
    const before = await page.evaluate(() => document.querySelectorAll('link[rel="stylesheet"]').length);
    await page.evaluate(() => window.__loadLazy());
    await page.waitForFunction(() => document.querySelector(".lazy"), { timeout: 10000 });
    const lazyColor = await page.evaluate(() => getComputedStyle(document.querySelector(".lazy")).color);
    assert.equal(lazyColor, "rgb(7, 8, 9)", "lazy stylesheet did not load with its chunk");
    const after = await page.evaluate(() => document.querySelectorAll('link[rel="stylesheet"]').length);
    assert.equal(after, before + 1, "dynamic import should add exactly one stylesheet link");
    assert.equal(errors.length, 0, `page errors: ${errors.join("|")}`);
  } finally {
    await browser.close();
    srv.kill("SIGKILL");
  }

  console.log("CSS-CODE-SPLIT E2E PASSED");
} catch (err) {
  failed = true;
  console.error("CSS-CODE-SPLIT E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
