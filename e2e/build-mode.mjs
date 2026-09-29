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

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-mode-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "mode-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, ".env.production"), "VITE_FLAVOR=prod-flavor\n");
// Vite's documented way to get a development build: NODE_ENV=development in .env.development.
fs.writeFileSync(path.join(app, ".env.development"), "VITE_FLAVOR=dev-flavor\nNODE_ENV=development\n");
fs.writeFileSync(path.join(app, ".env.staging"), "VITE_FLAVOR=staging-flavor\n");
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `window.__MODE = import.meta.env.MODE;\nwindow.__FLAVOR = import.meta.env.VITE_FLAVOR;\n` +
    `window.__DEV = import.meta.env.DEV;\nwindow.__PROD = import.meta.env.PROD;\n` +
    `window.__NODE_ENV = process.env.NODE_ENV;\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

async function built(mode, shellEnv = {}) {
  fs.rmSync(path.join(app, "dist"), { recursive: true, force: true });
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  const env = { ...process.env, ...shellEnv };
  delete env.NODE_ENV;
  Object.assign(env, shellEnv);
  execSync(mode ? `${oj} build ${app} --mode ${mode}` : `${oj} build ${app}`, { stdio: "ignore", env });
  const srv = spawn(oj, ["preview", app, "--port", "5471"], { stdio: "ignore" });
  await waitUp("http://localhost:5471/");
  const browser = await chromium.launch();
  const page = await browser.newPage();
  try {
    await page.goto("http://localhost:5471/", { timeout: 30000 });
    await page.waitForFunction(() => window.__MODE !== undefined, { timeout: 10000 });
    return await page.evaluate(() => ({
      mode: window.__MODE, flavor: window.__FLAVOR, dev: window.__DEV, prod: window.__PROD, nodeEnv: window.__NODE_ENV,
    }));
  } finally {
    await browser.close();
    srv.kill("SIGKILL");
    await sleep(300);
  }
}

let failed = false;
try {
  const dev = await built("development");
  assert.equal(dev.mode, "development", `--mode development sets import.meta.env.MODE`);
  assert.equal(dev.flavor, "dev-flavor", "--mode development loads .env.development");
  assert.equal(dev.dev, true, ".env.development NODE_ENV=development makes DEV true");
  assert.equal(dev.prod, false, "and PROD false");
  assert.equal(dev.nodeEnv, "development", "and process.env.NODE_ENV development");

  const prod = await built(null);
  assert.equal(prod.mode, "production", "default build mode is production");
  assert.equal(prod.flavor, "prod-flavor", "default build loads .env.production");
  assert.equal(prod.dev, false, "default build is not DEV");
  assert.equal(prod.nodeEnv, "production", "default build NODE_ENV production");

  // A mode with no NODE_ENV in its .env file stays a production build (Vite).
  const staging = await built("staging");
  assert.equal(staging.mode, "staging", "--mode staging sets MODE");
  assert.equal(staging.flavor, "staging-flavor", "--mode staging loads .env.staging");
  assert.equal(staging.dev, false, "no NODE_ENV in .env.staging: PROD build");
  assert.equal(staging.nodeEnv, "production");

  // The shell's NODE_ENV wins over the .env file.
  const forced = await built("development", { NODE_ENV: "production" });
  assert.equal(forced.mode, "development", "MODE still follows --mode");
  assert.equal(forced.dev, false, "shell NODE_ENV=production beats .env.development");
  assert.equal(forced.nodeEnv, "production");

  console.log("BUILD-MODE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("BUILD-MODE E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
