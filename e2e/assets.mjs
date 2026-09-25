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

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-assets-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "asset-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "src", "pic.svg"),
  `<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10"/></svg>`,
);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import picUrl from "./pic.svg";\n` +
    `const asUrl = new URL("./pic.svg", import.meta.url);\n` +
    `window.__PIC = picUrl;\n` +
    `window.__ASURL = asUrl.href;\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

async function check(port) {
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  const args = ["dev", app, "--port", String(port)];
  const srv = spawn(oj, args, { stdio: "ignore" });
  for (let i = 0; i < 80; i++) { try { if ((await fetch(`http://localhost:${port}/`)).ok) break; } catch {} await sleep(200); }
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${port}/`, { timeout: 30000 });
    await page.waitForFunction(() => window.__PIC !== undefined && window.__ASURL !== undefined, { timeout: 10000 });
    const pic = await page.evaluate(() => window.__PIC);
    const asurl = await page.evaluate(() => window.__ASURL);
    const svgStatus = await page.evaluate(async (u) => (await fetch(u)).status, pic);
    const bad = [];
    if (pic !== "/src/pic.svg") bad.push(`bare import url ${pic}`);
    if (!asurl || !asurl.endsWith("/src/pic.svg")) bad.push(`new URL ${asurl}`);
    if (svgStatus !== 200) bad.push(`asset fetch ${svgStatus}`);
    if (errors.length) bad.push(`page errors ${errors.join("|")}`);
    if (bad.length) throw new Error(`[dev] ${bad.join("; ")}`);
    console.log(`[dev] pic=${pic} newURL=${asurl} fetch=${svgStatus} OK`);
  } finally {
    await browser.close();
    srv.kill("SIGKILL");
  }
}

let failed = false;
try {
  await check(5293);
  console.log("ASSETS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("ASSETS E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
