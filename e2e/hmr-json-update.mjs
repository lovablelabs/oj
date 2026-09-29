// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Editing a JSON file a component imports must reach the page hot: the update
// names the importing boundary, so the re-fetched boundary has to import the JSON
// module's NEW version (`?t=<stamp>`, as Vite's importAnalysis stamps it) instead
// of the browser's cached instance. The fixture lives under playground/ so react
// resolves from its node_modules.

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
const PORT = 6111;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = path.join(repo, "playground", ".e2e-hmr-json-update");
fs.rmSync(app, { recursive: true, force: true });
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "hmr-json", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "src", "main.tsx"),
  `import { createRoot } from "react-dom/client";\nimport { App } from "./App";\n` +
    `createRoot(document.getElementById("root")!).render(<App />);\n`,
);
const DATA = path.join(app, "src", "data.json");
fs.writeFileSync(DATA, JSON.stringify({ label: "v1" }));
fs.writeFileSync(
  path.join(app, "src", "App.tsx"),
  `import { useState } from "react";\nimport data from "./data.json";\n` +
    `export function App() {\n  const [n, setN] = useState(0);\n` +
    `  return (<div><h1>{data.label}</h1><button onClick={() => setN(n + 1)}>Clicks: {n}</button></div>);\n}\n`,
);
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
    await page.locator("h1", { hasText: "v1" }).waitFor({ timeout: 20000 });
    const btn = page.locator("button");
    await btn.click();
    await btn.click();
    await page.evaluate(() => {
      window.__NOT_RELOADED = true;
    });

    fs.writeFileSync(DATA, JSON.stringify({ label: "v2" }));
    await page.locator("h1", { hasText: "v2" }).waitFor({ timeout: 20000 });
    assert.equal(await page.evaluate(() => window.__NOT_RELOADED), true, "page was reloaded");
    assert.match(await btn.textContent(), /Clicks: 2/, "component state lost");
    assert.equal(errors.length, 0, `page errors: ${errors.join("|")}`);

    // The served boundary names the JSON module's new version, not its bare url.
    const appJs = await (await fetch(`http://localhost:${PORT}/src/App.tsx?t=${Date.now()}`)).text();
    assert.match(appJs, /"\/src\/data\.json\?t=\d+"/, `App.tsx import of data.json is unstamped:\n${appJs}`);
    console.log("HMR-JSON-UPDATE E2E PASSED");
  } finally {
    await browser.close();
  }
} catch (err) {
  failed = true;
  console.error("HMR-JSON-UPDATE E2E FAILED:", err && err.stack ? err.stack : err);
} finally {
  srv.kill();
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
