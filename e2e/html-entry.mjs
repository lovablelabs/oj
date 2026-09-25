// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-htmlentry-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "he-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "index.tsx"), `window.__OK = "relative-entry-works";\n`);
// relative src, no leading slash (Vite accepts this; oj used to reject it)
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html lang="en"><head><title>t</title></head><body><div id="app"></div><script type="module" src="src/index.tsx"></script></body></html>`,
);

async function check(port) {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${port}/`, { timeout: 30000 });
    const ok = await page
      .waitForFunction(() => window.__OK, { timeout: 10000 })
      .then(() => page.evaluate(() => window.__OK));
    if (ok !== "relative-entry-works") throw new Error(`entry did not run: ${ok}`);
    if (errors.length) throw new Error(`page errors: ${errors.join("|")}`);
  } finally {
    await browser.close();
  }
}

async function mode(label, args, port, build) {
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  if (build) {
    fs.rmSync(path.join(app, "dist"), { recursive: true, force: true });
    execSync(`${oj} build ${app}`, { stdio: "ignore" });
    const html = fs.readFileSync(path.join(app, "dist", "index.html"), "utf8");
    if (!/src="\/assets\/index-.*\.js"/.test(html))
      throw new Error(`built html did not rewrite relative entry to hashed asset: ${html}`);
  }
  const srv = spawn(oj, args, { stdio: "ignore" });
  for (let i = 0; i < 80; i++) { try { if ((await fetch(`http://localhost:${port}/`)).ok) break; } catch {} await sleep(200); }
  try {
    await check(port);
    console.log(`[${label}] relative entry OK`);
  } finally {
    srv.kill("SIGKILL");
    await sleep(300);
  }
}

let failed = false;
try {
  await mode("dev", ["dev", app, "--port", "5411"], 5411, false);
  await mode("prod", ["preview", app, "--port", "5413"], 5413, true);
  console.log("HTML-ENTRY E2E PASSED");
} catch (err) {
  failed = true;
  console.error("HTML-ENTRY E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
