// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Writing an import before the file exists is a common flow. The transform must
// fail like Vite's import analysis ("Failed to resolve import ... Does the file
// exist?": a 500 and an error overlay pointing at the import), and creating the
// file must bring the page up without a manual reload: the server remembers the
// importers whose resolve failed and re-processes them when a new file appears
// (Vite's _hasResolveFailedErrorModules on the `create` event). The fixture lives
// under playground/ so react resolves from its node_modules.

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
const PORT = 6112;
const DIALOG = 'div[role="dialog"][aria-label="Build error"]';

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = path.join(repo, "playground", ".e2e-hmr-unresolved-import");
fs.rmSync(app, { recursive: true, force: true });
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "unresolved-import", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "src", "main.tsx"),
  `import { createRoot } from "react-dom/client";\nimport { App } from "./App";\n` +
    `createRoot(document.getElementById("root")!).render(<App />);\n`,
);
const APP = path.join(app, "src", "App.tsx");
const appSource = (extra) =>
  `import { useState } from "react";\nimport { Later } from "./Later";\n${extra}` +
  `export function App() {\n  const [n, setN] = useState(0);\n` +
  `  return (<div><h1>app</h1><Later />${extra ? "<Extra />" : ""}<button onClick={() => setN(n + 1)}>Clicks: {n}</button></div>);\n}\n`;
fs.writeFileSync(APP, appSource(""));
const LATER = path.join(app, "src", "Later.tsx");
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>`,
);

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${PORT}/`);

  // The importer is a 500 with Vite's message, not a 200 shipping "./Later" as-is.
  const res = await fetch(`http://localhost:${PORT}/src/App.tsx`);
  const body = await res.text();
  assert.equal(res.status, 500, `expected 500 for an unresolved import, got ${res.status}:\n${body}`);
  assert.match(body, /Failed to resolve import "\.\/Later" from "src\/App\.tsx"\. Does the file exist\?/, body);
  assert.match(body, /src\/App\.tsx:2:24/, `error does not point at the import specifier:\n${body}`);
  console.log("500 + resolve error:  yes");

  // A bare specifier no package or plugin answers fails the same way (Vite
  // does not ship the bare name for the browser to reject with its own error).
  fs.writeFileSync(
    path.join(app, "src", "Bare.tsx"),
    `import missing from "not-installed-pkg";\nexport const bare = missing;\n`,
  );
  const bareRes = await fetch(`http://localhost:${PORT}/src/Bare.tsx`);
  const bareBody = await bareRes.text();
  assert.equal(
    bareRes.status,
    500,
    `expected 500 for an unresolvable bare import, got ${bareRes.status}:\n${bareBody}`,
  );
  assert.match(
    bareBody,
    /Failed to resolve import "not-installed-pkg" from "src\/Bare\.tsx"\. Does the file exist\?/,
    bareBody,
  );
  assert.match(bareBody, /src\/Bare\.tsx:1:22/, `error does not point at the bare specifier:\n${bareBody}`);
  console.log("bare import error:    yes");
  // Make it valid again so the failed-importer retry below has one module to
  // recover (the page never imports Bare.tsx; only App.tsx is under test).
  fs.writeFileSync(path.join(app, "src", "Bare.tsx"), `export const bare = 1;\n`);
  await sleep(500);

  const browser = await chromium.launch();
  const page = await browser.newPage();
  try {
    await page.goto(`http://localhost:${PORT}/`, { waitUntil: "networkidle", timeout: 30000 });
    await page.waitForSelector(DIALOG, { timeout: 15000 });
    const frame = (await page.locator(`${DIALOG} pre`).textContent()) || "";
    assert.match(frame, /Failed to resolve import "\.\/Later"/, `overlay frame:\n${frame}`);
    console.log("overlay on open:      yes");

    // Creating the missing file recovers the page (the module script never ran,
    // so this is a reload), with no manual refresh.
    fs.writeFileSync(LATER, `export function Later() {\n  return <p>later v1</p>;\n}\n`);
    await page.locator("p", { hasText: "later v1" }).waitFor({ timeout: 20000 });
    await page.waitForSelector(DIALOG, { state: "detached", timeout: 5000 });
    console.log("create recovers:      yes");

    // The recovered page hot-updates normally afterwards.
    const btn = page.locator("button");
    await btn.click();
    fs.writeFileSync(LATER, `export function Later() {\n  return <p>later v2</p>;\n}\n`);
    await page.locator("p", { hasText: "later v2" }).waitFor({ timeout: 20000 });
    assert.match(await btn.textContent(), /Clicks: 1/, "hot update after recovery lost state");
    console.log("then hot update:      yes");

    // On a running page: add an import of a file that does not exist yet, then
    // create it. The overlay shows in between and clears once the file exists.
    fs.writeFileSync(APP, appSource(`import { Extra } from "./Extra";\n`));
    await page.waitForSelector(DIALOG, { timeout: 15000 });
    const frame2 = (await page.locator(`${DIALOG} pre`).textContent()) || "";
    assert.match(frame2, /Failed to resolve import "\.\/Extra" from "src\/App\.tsx"/, `overlay frame:\n${frame2}`);
    fs.writeFileSync(path.join(app, "src", "Extra.tsx"), `export function Extra() {\n  return <p>extra</p>;\n}\n`);
    await page.locator("p", { hasText: "extra" }).waitFor({ timeout: 20000 });
    await page.waitForSelector(DIALOG, { state: "detached", timeout: 5000 });
    console.log("live edit + create:   yes");
    console.log("HMR-UNRESOLVED-IMPORT E2E PASSED");
  } finally {
    await browser.close();
  }
} catch (err) {
  failed = true;
  console.error("HMR-UNRESOLVED-IMPORT E2E FAILED:", err && err.stack ? err.stack : err);
} finally {
  srv.kill();
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
