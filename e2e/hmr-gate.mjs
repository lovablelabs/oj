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

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-gate-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "gate-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "main.js"), `document.title = "v1"; window.__READY = true;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

const port = 5488;
let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(port)], {
  stdio: "ignore",
  env: { ...process.env, OJ_HMR_GATE: "1" },
});
try {
  await waitUp(`http://localhost:${port}/`);

  // on-connect custom frame: dev-server-mode
  const ws = new WebSocket(`ws://localhost:${port}/__ws`);
  const frames = {};
  const onConnect = new Promise((res, rej) => {
    const t = setTimeout(() => rej(new Error("missing on-connect frames")), 6000);
    ws.addEventListener("message", (e) => {
      let m;
      try {
        m = JSON.parse(e.data);
      } catch {
        return;
      }
      if (m.type === "custom" && (m.event === "lovable:dev-server-mode" || m.event === "lovable:boot-progress")) {
        frames[m.event] = m.data;
        if (frames["lovable:dev-server-mode"] && frames["lovable:boot-progress"]) {
          clearTimeout(t);
          res();
        }
      }
    });
  });
  await onConnect;
  assert.equal(frames["lovable:dev-server-mode"].mode, "classic", "gate announces classic mode on connect");
  const boot = frames["lovable:boot-progress"];
  assert.equal(typeof boot.ssrModules, "number", "boot-progress ssrModules is a number");
  assert.equal(typeof boot.clientModules, "number", "boot-progress clientModules is a number");
  ws.close();

  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${port}/`, { timeout: 30000 });
    await page.waitForFunction(() => window.__READY === true, { timeout: 10000 });
    await page.evaluate(() => {
      window.__MARK = "kept";
    });
    assert.equal(await page.title(), "v1");

    // edit while gated: HMR must be HELD (no reload, title unchanged, marker intact)
    fs.writeFileSync(path.join(app, "src", "main.js"), `document.title = "v2"; window.__READY = true;\n`);
    await sleep(1500);
    assert.equal(await page.title(), "v1", "edit is held: title unchanged before flush");
    assert.equal(await page.evaluate(() => window.__MARK), "kept", "no reload happened before flush");

    // gate status shows the pending file
    const status = await (await fetch(`http://localhost:${port}/__hmr_gate`)).json();
    assert.equal(status.enabled, true, "gate enabled");
    assert.ok(status.count >= 1, `gate has pending changes (count=${status.count})`);

    // flush releases -> full-reload
    const flush = await (await fetch(`http://localhost:${port}/__hmr_flush`, { method: "POST" })).json();
    assert.equal(flush.mode, "full-reload", "flush reports full-reload mode");
    assert.ok(flush.count >= 1, `flush released changes (count=${flush.count})`);

    await page.waitForFunction(() => document.title === "v2", { timeout: 10000 });
    assert.equal(await page.evaluate(() => window.__MARK), undefined, "page reloaded on flush (marker gone)");
    assert.equal(errors.length, 0, `page errors: ${errors.join("|")}`);
  } finally {
    await browser.close();
  }
  console.log("HMR-GATE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("HMR-GATE E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
