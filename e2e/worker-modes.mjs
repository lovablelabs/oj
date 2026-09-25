// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-worker-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "worker-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "worker.js"), `self.onmessage = (e) => { self.postMessage(e.data * 2); };\n`);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import Work from "./worker.js?worker";\nconst w = new Work();\nw.onmessage = (e) => { window.__W = e.data; };\nw.postMessage(21);\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

async function check(port) {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${port}/`, { timeout: 30000 });
    const w = await page
      .waitForFunction(() => window.__W !== undefined, { timeout: 10000 })
      .then(() => page.evaluate(() => window.__W));
    if (w !== 42) throw new Error(`worker replied ${w}, expected 42`);
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
    const chunks = fs.readdirSync(path.join(app, "dist", "assets"));
    if (!chunks.some((f) => f.startsWith("worker-") && f.endsWith(".js")))
      throw new Error(`worker not emitted as a separate chunk: ${chunks.join(",")}`);
  }
  const srv = spawn(oj, args, { stdio: "ignore" });
  for (let i = 0; i < 80; i++) { try { if ((await fetch(`http://localhost:${port}/`)).ok) break; } catch {} await sleep(200); }
  try {
    await check(port);
    console.log(`[${label}] worker OK`);
  } finally {
    srv.kill("SIGKILL");
    await sleep(300);
  }
}

let failed = false;
try {
  await mode("dev", ["dev", app, "--port", "5402"], 5402, false);
  await mode("prod", ["preview", app, "--port", "5404"], 5404, true);
  console.log("WORKER-MODES E2E PASSED");
} catch (err) {
  failed = true;
  console.error("WORKER-MODES E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
