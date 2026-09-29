// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// A page opened while a leaf component has a syntax error must show the error
// overlay (the error frame is broadcast before the page's socket exists, so the
// server has to hold it for the next client, like Vite's ws `bufferedError`), and
// fixing the file must bring the app up. The boundary above the leaf never
// evaluated, so there is nothing to hot-swap into: the client reloads on that
// first update instead of dropping it (Vite's clearOverlayOrReloadOnFirstUpdate).
// The fixture lives under playground/ so react resolves from its node_modules.

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
const PORT = 6110;
const DIALOG = 'div[role="dialog"][aria-label="Build error"]';

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = path.join(repo, "playground", ".e2e-hmr-overlay-recovery");
fs.rmSync(app, { recursive: true, force: true });
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "overlay-recovery", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "src", "main.tsx"),
  `import { createRoot } from "react-dom/client";\nimport { App } from "./App";\n` +
    `createRoot(document.getElementById("root")!).render(<App />);\n`,
);
fs.writeFileSync(
  path.join(app, "src", "App.tsx"),
  `import { Leaf } from "./Leaf";\nexport function App() {\n  return (<div><h1>app</h1><Leaf /></div>);\n}\n`,
);
const LEAF = path.join(app, "src", "Leaf.tsx");
const GOOD = `export function Leaf() {\n  return <p>leaf ok</p>;\n}\n`;
fs.writeFileSync(LEAF, GOOD + `const __broken_syntax = ;\n`);
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
  try {
    await page.goto(`http://localhost:${PORT}/`, { waitUntil: "networkidle", timeout: 30000 });
    // The 500 on /src/Leaf.tsx happened before the HMR socket opened: the overlay
    // must still appear, from the buffered frame.
    await page.waitForSelector(DIALOG, { timeout: 15000 });
    const frame = (await page.locator(`${DIALOG} pre`).textContent()) || "";
    assert.match(frame, /Leaf\.tsx/, `overlay frame does not point at Leaf.tsx:\n${frame}`);
    assert.equal(await page.locator("h1").count(), 0, "app rendered despite the compile error");
    console.log("startup overlay:     yes");

    await page.evaluate(() => { window.__OJ_STALE_PAGE = true; });
    fs.writeFileSync(LEAF, GOOD);
    // Recovery is a reload (the page's modules never loaded), not a silent drop.
    await page.locator("p", { hasText: "leaf ok" }).waitFor({ timeout: 20000 });
    await page.waitForSelector(DIALOG, { state: "detached", timeout: 5000 });
    assert.equal(await page.evaluate(() => window.__OJ_STALE_PAGE), undefined, "page was not reloaded");
    console.log("recovered by reload: yes");

    // A later edit on the running page is a normal hot update, not a reload.
    await page.evaluate(() => { window.__OJ_LIVE_PAGE = true; });
    fs.writeFileSync(LEAF, GOOD.replace("leaf ok", "leaf v2"));
    await page.locator("p", { hasText: "leaf v2" }).waitFor({ timeout: 20000 });
    assert.equal(await page.evaluate(() => window.__OJ_LIVE_PAGE), true, "second edit reloaded the page");
    console.log("then hot update:     yes");
    console.log("HMR-OVERLAY-RECOVERY E2E PASSED");
  } finally {
    await browser.close();
  }
} catch (err) {
  failed = true;
  console.error("HMR-OVERLAY-RECOVERY E2E FAILED:", err && err.stack ? err.stack : err);
} finally {
  srv.kill();
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
