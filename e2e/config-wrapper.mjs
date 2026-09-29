// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

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

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-wrapper-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "wrapper-app", version: "1.0.0" }));

const preset = path.join(app, "node_modules", "oj-preset");
fs.mkdirSync(preset, { recursive: true });
fs.writeFileSync(path.join(preset, "package.json"), JSON.stringify({ name: "oj-preset", version: "1.0.0", type: "module", main: "index.mjs" }));
fs.writeFileSync(
  path.join(preset, "index.mjs"),
  `export function defineConfig(user = {}) {
     return {
       ...user,
       resolve: { ...(user.resolve || {}), alias: { "@": process.cwd() + "/src" } },
       plugins: [
         ...(user.plugins || []),
         { name: "oj-preset", transformIndexHtml(h) { return h.replace("</head>", '<script>window.__PRESET = 1;</script></head>'); } },
       ],
     };
   }\n`,
);

fs.writeFileSync(path.join(app, "vite.config.mjs"), `import { defineConfig } from "oj-preset";\nexport default defineConfig({ plugins: [] });\n`);
fs.writeFileSync(path.join(app, "src", "thing.js"), `export const V = "aliased-ok";\n`);
fs.writeFileSync(path.join(app, "main.js"), `import { V } from "@/thing.js";\nwindow.__V = V;\nwindow.__READY = true;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/main.js"></script></body></html>`,
);

const port = 5498;
let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${port}/`);

  const html = await (await fetch(`http://localhost:${port}/`)).text();
  assert.match(html, /window\.__PRESET = 1/, "wrapper's transformIndexHtml plugin ran (config-wrapper evaluated)");

  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${port}/`, { timeout: 30000 });
    await page.waitForFunction(() => window.__READY === true, { timeout: 10000 });
    const v = await page.evaluate(() => window.__V);
    assert.equal(v, "aliased-ok", "wrapper's resolve.alias (@ -> src) applied");
    assert.equal(errors.length, 0, `page errors: ${errors.join("|")}`);
  } finally {
    await browser.close();
  }
  console.log("CONFIG-WRAPPER E2E PASSED");
} catch (err) {
  failed = true;
  console.error("CONFIG-WRAPPER E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
