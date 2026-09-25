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

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-preproc-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "preproc-app", version: "1.0.0" }));
try {
  execSync("npm install less stylus --no-audit --no-fund --loglevel=error", { cwd: app, stdio: "ignore" });
} catch {
  console.log("SKIP preprocessors: could not install less/stylus (offline?)");
  fs.rmSync(app, { recursive: true, force: true });
  process.exit(0);
}

fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "src", "a.less"), `@c: rgb(10, 20, 30);\n.box { color: @c; }\n`);
fs.writeFileSync(path.join(app, "src", "b.styl"), `d = rgb(40, 50, 60)\n.boxs\n  color d\n`);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import "./a.less";\nimport "./b.styl";\n` +
    `const a = document.createElement("div"); a.className = "box"; a.id = "a"; document.body.appendChild(a);\n` +
    `const b = document.createElement("div"); b.className = "boxs"; b.id = "b"; document.body.appendChild(b);\n` +
    `window.__READY = true;\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

async function colors(port) {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${port}/`, { timeout: 30000 });
    await page.waitForFunction(() => window.__READY === true, { timeout: 10000 });
    const r = await page.evaluate(() => ({
      a: getComputedStyle(document.getElementById("a")).color,
      b: getComputedStyle(document.getElementById("b")).color,
    }));
    if (r.a !== "rgb(10, 20, 30)") throw new Error(`less color ${r.a}`);
    if (r.b !== "rgb(40, 50, 60)") throw new Error(`stylus color ${r.b}`);
    if (errors.length) throw new Error(`page errors ${errors.join("|")}`);
  } finally {
    await browser.close();
  }
}

async function run(label, args, port, isBuild) {
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  if (isBuild) {
    fs.rmSync(path.join(app, "dist"), { recursive: true, force: true });
    execSync(`${oj} build ${app}`, { stdio: "ignore" });
  }
  const srv = spawn(oj, args, { stdio: "ignore" });
  for (let i = 0; i < 80; i++) { try { if ((await fetch(`http://localhost:${port}/`)).ok) break; } catch {} await sleep(200); }
  try {
    await colors(port);
    console.log(`[${label}] less + stylus OK`);
  } finally {
    srv.kill("SIGKILL");
    await sleep(300);
  }
}

let failed = false;
try {
  await run("dev", ["dev", app, "--port", "5342"], 5342, false);
  await run("prod", ["preview", app, "--port", "5344"], 5344, true);
  console.log("PREPROCESSORS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PREPROCESSORS E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
