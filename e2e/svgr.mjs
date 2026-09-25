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

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-svgr-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "svgr-app", version: "1.0.0" }));
try {
  execSync("npm install react react-dom --no-audit --no-fund --loglevel=error", { cwd: app, stdio: "ignore" });
} catch {
  console.log("SKIP svgr: could not install react (offline?)");
  fs.rmSync(app, { recursive: true, force: true });
  process.exit(0);
}

fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(
  path.join(app, "src", "icon.svg"),
  `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" stroke-width="2" class="base"><path d="M4 4h16" stroke="currentColor"/></svg>`,
);
fs.writeFileSync(
  path.join(app, "src", "main.tsx"),
  `import { createRoot } from "react-dom/client";\nimport Icon from "./icon.svg?react";\n` +
    `const el = document.createElement("div"); el.id = "root"; document.body.appendChild(el);\n` +
    `createRoot(el).render(<Icon className="added" data-testid="icon" />);\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.tsx"></script></body></html>`,
);

async function check(port) {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${port}/`, { timeout: 30000 });
    const el = await page.waitForSelector("#root svg", { timeout: 10000 });
    const outer = await page.evaluate((e) => e.outerHTML, el);
    if (!/^<svg/.test(outer)) throw new Error("no svg rendered");
    if (!/class="added"/.test(outer)) throw new Error(`props not spread onto svg: ${outer.slice(0, 80)}`);
    if (!/stroke-width="2"/.test(outer)) throw new Error("attribute rewrite lost stroke-width");
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
  }
  const srv = spawn(oj, args, { stdio: "ignore" });
  for (let i = 0; i < 80; i++) { try { if ((await fetch(`http://localhost:${port}/`)).ok) break; } catch {} await sleep(200); }
  try {
    await check(port);
    console.log(`[${label}] svg component OK`);
  } finally {
    srv.kill("SIGKILL");
    await sleep(300);
  }
}

let failed = false;
try {
  await mode("dev", ["dev", app, "--port", "5392"], 5392, false);
  await mode("prod", ["preview", app, "--port", "5394"], 5394, true);
  console.log("SVGR E2E PASSED");
} catch (err) {
  failed = true;
  console.error("SVGR E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
