// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// create-vite's template imports its logo from the public dir by URL
// (`import viteLogo from '/vite.svg'`) and app code can import `/src/x`.
// `oj build` must resolve both, as Vite does: `/src/x` from the root, the
// public file to its own URL under `base`. Built here under an absolute and a
// relative base, then served by `oj preview` and loaded in a browser.

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
const PORT = 6115;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-root-imports-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.mkdirSync(path.join(app, "public"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "root-imports", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "public", "vite.svg"),
  `<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32"><rect width="32" height="32" fill="red"/></svg>`,
);
fs.writeFileSync(path.join(app, "src", "label.js"), `export const label = "root-absolute-import";\n`);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import viteLogo from "/vite.svg";\nimport { label } from "/src/label.js";\n` +
    `const img = document.createElement("img");\nimg.id = "logo";\nimg.src = viteLogo;\ndocument.body.append(img);\n` +
    `const h1 = document.createElement("h1");\nh1.textContent = label;\ndocument.body.append(h1);\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

let failed = false;
const browser = await chromium.launch();
try {
  for (const base of ["/app/", "./"]) {
    fs.writeFileSync(path.join(app, "oj.config.json"), JSON.stringify({ base }));
    execSync(`"${oj}" build`, { cwd: app, stdio: "inherit" });
    const pagePath = base === "./" ? "/" : base;
    const srv = spawn(oj, ["preview", app, "--port", String(PORT)], { stdio: "ignore" });
    try {
      await waitUp(`http://localhost:${PORT}${pagePath}`);
      const page = await browser.newPage();
      const errors = [];
      page.on("pageerror", (e) => errors.push(String(e)));
      await page.goto(`http://localhost:${PORT}${pagePath}`, { waitUntil: "networkidle", timeout: 30000 });
      await page.locator("h1", { hasText: "root-absolute-import" }).waitFor({ timeout: 10000 });
      const width = await page.evaluate(() => document.getElementById("logo").naturalWidth);
      assert.equal(width, 32, `base ${base}: the public logo did not load`);
      assert.equal(errors.length, 0, `base ${base}: page errors: ${errors.join("|")}`);
      await page.close();
    } finally {
      srv.kill();
      await new Promise((r) => srv.once("exit", r));
    }
  }
  console.log("BUILD-ROOT-IMPORTS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("BUILD-ROOT-IMPORTS E2E FAILED:", err && err.stack ? err.stack : err);
} finally {
  await browser.close();
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
