// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// A non-root `base`: the page loads oj's HMR client and React refresh preamble
// from base-prefixed URLs (as Vite serves /base/@vite/client), so both must
// answer there. Without them the socket never connects and every Fast Refresh
// boundary throws. Checks a component edit lands hot with state kept, and a
// visit to `/` redirects to the base. The fixture lives under playground/ so
// react resolves from its node_modules.

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
const PORT = 6114;
const ORIGIN = `http://localhost:${PORT}`;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = path.join(repo, "playground", ".e2e-dev-base-hmr");
fs.rmSync(app, { recursive: true, force: true });
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "dev-base-hmr", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "oj.config.json"), JSON.stringify({ base: "/app/" }));
fs.writeFileSync(
  path.join(app, "src", "main.tsx"),
  `import { createRoot } from "react-dom/client";\nimport { App } from "./App";\n` +
    `createRoot(document.getElementById("root")!).render(<App />);\n`,
);
const APP = path.join(app, "src", "App.tsx");
const component = (label) =>
  `import { useState } from "react";\n` +
  `export function App() {\n  const [n, setN] = useState(0);\n` +
  `  return (<div><h1>${label}</h1><button onClick={() => setN(n + 1)}>Clicks: {n}</button></div>);\n}\n`;
fs.writeFileSync(APP, component("v1"));
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>`,
);

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
try {
  await waitUp(`${ORIGIN}/app/`);

  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  const missing = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  page.on("response", (r) => r.status() === 404 && missing.push(r.url()));
  try {
    await page.goto(`${ORIGIN}/app/`, { waitUntil: "networkidle", timeout: 30000 });
    await page.locator("h1", { hasText: "v1" }).waitFor({ timeout: 20000 });
    assert.deepEqual(missing, [], "requests 404ed under the base");
    const btn = page.locator("button");
    await btn.click();
    await btn.click();
    await page.evaluate(() => {
      window.__NOT_RELOADED = true;
    });

    fs.writeFileSync(APP, component("v2"));
    await page.locator("h1", { hasText: "v2" }).waitFor({ timeout: 20000 });
    assert.equal(await page.evaluate(() => window.__NOT_RELOADED), true, "page was reloaded");
    assert.match(await btn.textContent(), /Clicks: 2/, "component state lost");
    assert.equal(errors.length, 0, `page errors: ${errors.join("|")}`);
    const visit = await fetch(`${ORIGIN}/`, { redirect: "manual", headers: { accept: "text/html" } });
    assert.equal(visit.status, 302, "visit to / not redirected");
    assert.equal(visit.headers.get("location"), "/app/");
    console.log("DEV-BASE-HMR E2E PASSED");
  } finally {
    await browser.close();
  }
} catch (err) {
  failed = true;
  console.error("DEV-BASE-HMR E2E FAILED:", err && err.stack ? err.stack : err);
} finally {
  srv.kill();
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
