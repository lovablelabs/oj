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

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-hmrproto-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "hmr-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "main.js"), `document.title = "v1"; window.__READY = true;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

async function run(args, port, hmrAsset) {
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  const srv = spawn(oj, args, { stdio: "ignore" });
  for (let i = 0; i < 80; i++) { try { if ((await fetch(`http://localhost:${port}/`)).ok) break; } catch {} await sleep(200); }
  try {
    // the served HMR client must derive the ws protocol from location.protocol
    // (so wss is used behind an https sandbox proxy instead of hardcoded ws://)
    const client = await (await fetch(`http://localhost:${port}/@oj/${hmrAsset}`)).text();
    assert.match(client, /location\.protocol === "https:" \? "wss" : "ws"/, `${hmrAsset} derives wss`);
    assert.ok(!/"ws:\/\/"\s*\+\s*location\.host/.test(client), "no hardcoded ws:// left");

    // 2. HMR still works over http: the socket connects and a full-reload lands
    const browser = await chromium.launch();
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", (e) => errors.push(String(e)));
    try {
      await page.goto(`http://localhost:${port}/`, { timeout: 30000 });
      await page.waitForFunction(() => window.__READY === true, { timeout: 10000 });
      fs.writeFileSync(path.join(app, "src", "main.js"), `document.title = "v2"; window.__READY = true;\n`);
      await page.waitForFunction(() => document.title === "v2", { timeout: 10000 });
      assert.equal(errors.length, 0, `page errors: ${errors.join("|")}`);
    } finally {
      await browser.close();
    }
  } finally {
    srv.kill("SIGKILL");
    await sleep(300);
  }
}

let failed = false;
try {
  await run(["dev", app, "--port", "5472"], 5472, "client.js");
  console.log("[dev] hmr protocol OK");
  console.log("HMR-PROTOCOL E2E PASSED");
} catch (err) {
  failed = true;
  console.error("HMR-PROTOCOL E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
