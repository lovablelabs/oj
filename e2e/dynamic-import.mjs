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

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-dynimp-"));
fs.mkdirSync(path.join(app, "src", "pages"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "dynimp-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "pages", "home.js"), `export const name = "home-page";\n`);
fs.writeFileSync(path.join(app, "src", "pages", "about.js"), `export const name = "about-page";\n`);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `async function load(which) { const m = await import(\`./pages/\${which}.js\`); return m.name; }\n` +
    `window.__LOAD = load;\n` +
    `load("home").then((n) => { window.__HOME = n; });\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

async function inBrowser(port) {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${port}/`, { timeout: 30000 });
    await page.waitForFunction(() => window.__HOME !== undefined, { timeout: 10000 });
    const home = await page.evaluate(() => window.__HOME);
    const about = await page.evaluate(async () => window.__LOAD("about"));
    if (home !== "home-page") throw new Error(`home=${home}`);
    if (about !== "about-page") throw new Error(`about=${about}`);
    if (errors.length) throw new Error(`page errors ${errors.join("|")}`);
  } finally {
    await browser.close();
  }
}

async function serve(cmd, extra, port) {
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  const srv = spawn(oj, [cmd, app, "--port", String(port), ...extra], { stdio: "ignore" });
  for (let i = 0; i < 80; i++) { try { if ((await fetch(`http://localhost:${port}/`)).ok) break; } catch {} await sleep(200); }
  return srv;
}

let failed = false;
try {
  let s = await serve("dev", [], 5307);
  await inBrowser(5307);
  s.kill("SIGKILL");
  console.log("[dev] OK");

  fs.rmSync(path.join(app, "dist"), { recursive: true, force: true });
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const chunks = fs.readdirSync(path.join(app, "dist", "assets"));
  if (!chunks.some((f) => f.startsWith("home-")) || !chunks.some((f) => f.startsWith("about-")))
    throw new Error(`prod build did not code-split pages: ${chunks.join(",")}`);
  s = await serve("preview", [], 5309);
  await inBrowser(5309);
  s.kill("SIGKILL");
  console.log("[prod build + preview] OK");

  console.log("DYNAMIC-IMPORT E2E PASSED");
} catch (err) {
  failed = true;
  console.error("DYNAMIC-IMPORT E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
