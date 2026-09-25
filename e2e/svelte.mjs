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

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-svelte-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "svelte-app", version: "1.0.0" }));
try {
  execSync("npm install svelte --no-audit --no-fund --loglevel=error", { cwd: app, stdio: "ignore" });
} catch {
  console.log("SKIP svelte: could not install svelte (offline?)");
  fs.rmSync(app, { recursive: true, force: true });
  process.exit(0);
}

fs.mkdirSync(path.join(app, "src"), { recursive: true });
const appSvelte = (label) =>
  `<script>\n  let count = $state(0);\n</script>\n<button id="btn" onclick={() => count++}>${label}: {count}</button>\n`;
fs.writeFileSync(path.join(app, "src", "App.svelte"), appSvelte("count"));
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import { mount } from "svelte";\nimport App from "./App.svelte";\nmount(App, { target: document.getElementById("app") });\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><div id="app"></div><script type="module" src="/src/main.js"></script></body></html>`,
);

async function renders(page) {
  const btn = await page.waitForSelector("#btn", { timeout: 10000 });
  if ((await page.evaluate((e) => e.textContent, btn)) !== "count: 0") throw new Error("initial render wrong");
  await btn.click();
  if ((await page.evaluate((e) => e.textContent, btn)) !== "count: 1") throw new Error("reactivity broken");
  return btn;
}

async function mode(label, args, port, build, hmr) {
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  fs.writeFileSync(path.join(app, "src", "App.svelte"), appSvelte("count"));
  if (build) {
    fs.rmSync(path.join(app, "dist"), { recursive: true, force: true });
    execSync(`${oj} build ${app}`, { stdio: "ignore" });
  }
  const srv = spawn(oj, args, { stdio: "ignore" });
  for (let i = 0; i < 80; i++) { try { if ((await fetch(`http://localhost:${port}/`)).ok) break; } catch {} await sleep(200); }
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${port}/`, { timeout: 30000 });
    await renders(page);
    if (hmr) {
      await page.evaluate(() => (window.__SURVIVE = "yes"));
      fs.writeFileSync(path.join(app, "src", "App.svelte"), appSvelte("clicks"));
      await page.waitForFunction(
        () => document.getElementById("btn")?.textContent?.startsWith("clicks:"),
        { timeout: 10000 },
      );
      const survived = await page.evaluate(() => window.__SURVIVE);
      if (survived !== "yes") throw new Error("HMR did a full reload instead of a hot swap");
    }
    if (errors.length) throw new Error(`page errors: ${errors.join("|")}`);
    console.log(`[${label}] svelte OK${hmr ? " (+hot swap, no reload)" : ""}`);
  } finally {
    await browser.close();
    srv.kill("SIGKILL");
    await sleep(300);
  }
}

let failed = false;
try {
  await mode("dev", ["dev", app, "--port", "5431"], 5431, false, true);
  await mode("prod", ["preview", app, "--port", "5433"], 5433, true, false);
  console.log("SVELTE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("SVELTE E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
