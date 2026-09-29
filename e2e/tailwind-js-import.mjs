// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Tailwind loaded the standard Vite way -- `import "./index.css"` from the entry,
// no <link> -- must hot-swap on a source edit: the css-update re-runs the css
// module wrapper (updateStyle) instead of reloading the page, so component state
// survives. The fixture lives under playground/ so react and tailwind resolve
// from its node_modules.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
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
const PORT = 5481;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = path.join(repo, "playground", ".e2e-tailwind-js-import");
fs.rmSync(app, { recursive: true, force: true });
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "tw-js-import", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "index.css"), `@import "tailwindcss";\n`);
fs.writeFileSync(
  path.join(app, "src", "main.tsx"),
  `import "./index.css";\nimport { createRoot } from "react-dom/client";\nimport { App } from "./App";\n` +
    `createRoot(document.getElementById("root")!).render(<App />);\n`,
);
const APP = path.join(app, "src", "App.tsx");
const appSrc = (cls) =>
  `import { useState } from "react";\n` +
  `export function App() {\n  const [n, setN] = useState(0);\n` +
  `  return (<div><h1 className="${cls}">hi</h1><button onClick={() => setN(n + 1)}>Clicks: {n}</button></div>);\n}\n`;
fs.writeFileSync(APP, appSrc("underline"));
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>`,
);

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${PORT}/`);
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${PORT}/`, { waitUntil: "networkidle", timeout: 30000 });
    const h1 = page.locator("h1");
    await h1.waitFor({ timeout: 20000 });
    await page.waitForFunction(
      () => getComputedStyle(document.querySelector("h1")).textDecorationLine === "underline",
      { timeout: 20000 },
    );
    assert.equal(await page.evaluate(() => document.querySelectorAll("link[rel=stylesheet]").length), 0, "no <link>: css is JS-imported");
    const btn = page.locator("button");
    await btn.click(); await btn.click();
    await page.evaluate(() => { window.__NOT_RELOADED = true; });

    fs.writeFileSync(APP, appSrc("underline italic"));
    await page.waitForFunction(
      () => getComputedStyle(document.querySelector("h1")).fontStyle === "italic",
      { timeout: 20000 },
    );
    assert.equal(await page.evaluate(() => window.__NOT_RELOADED), true, "page was reloaded");
    assert.match(await btn.textContent(), /Clicks: 2/, "component state lost");
    assert.equal(errors.length, 0, `page errors: ${errors.join("|")}`);
    console.log("TAILWIND-JS-IMPORT E2E PASSED");
  } finally {
    await browser.close();
  }
} catch (err) {
  failed = true;
  console.error("TAILWIND-JS-IMPORT E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
