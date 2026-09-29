// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Editing a module that is not itself an HMR boundary (a plain `.ts` util
// imported by a component) must reach the page: the re-fetched boundary has to
// import the util's NEW version (`?t=<stamp>`), not the browser's cached one, and
// the update must stay hot (state preserved). The fixture lives under
// playground/ so react resolves from its node_modules.

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
const PORT = 5482;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = path.join(repo, "playground", ".e2e-hmr-dep-update");
fs.rmSync(app, { recursive: true, force: true });
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "hmr-dep", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "src", "main.tsx"),
  `import { createRoot } from "react-dom/client";\nimport { App } from "./App";\n` +
    `createRoot(document.getElementById("root")!).render(<App />);\n`,
);
// App (boundary) -> hooks.ts (not a boundary) -> label.ts (not a boundary, edited)
fs.writeFileSync(
  path.join(app, "src", "hooks.ts"),
  `import { label } from "./label";\nexport const useLabel = () => label;\n`,
);
const LABEL = path.join(app, "src", "label.ts");
fs.writeFileSync(LABEL, `export const label = "v1";\n`);
fs.writeFileSync(
  path.join(app, "src", "App.tsx"),
  `import { useState } from "react";\nimport { useLabel } from "./hooks";\n` +
    `export function App() {\n  const [n, setN] = useState(0);\n  const label = useLabel();\n` +
    `  return (<div><h1>{label}</h1><button onClick={() => setN(n + 1)}>Clicks: {n}</button></div>);\n}\n`,
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

    fs.writeFileSync(LABEL, `export const label = "v2";\n`);
    await page.locator("h1", { hasText: "v2" }).waitFor({ timeout: 20000 });
    assert.equal(await page.evaluate(() => window.__NOT_RELOADED), true, "page was reloaded");
    assert.match(await btn.textContent(), /Clicks: 2/, "component state lost");
    assert.equal(errors.length, 0, `page errors: ${errors.join("|")}`);

    // The served boundary names the util's new version, not its bare (cached) url.
    const appJs = await (await fetch(`http://localhost:${PORT}/src/App.tsx?t=${Date.now()}`)).text();
    assert.match(appJs, /"\/src\/hooks\.ts\?t=\d+"/, `App.tsx import of hooks.ts is unstamped:\n${appJs}`);
    const hooksJs = await (await fetch(`http://localhost:${PORT}/src/hooks.ts?t=${Date.now()}`)).text();
    assert.match(hooksJs, /"\/src\/label\.ts\?t=\d+"/, `hooks.ts import of label.ts is unstamped:\n${hooksJs}`);
    const mainJs = await (await fetch(`http://localhost:${PORT}/src/main.tsx`)).text();
    assert.doesNotMatch(mainJs, /App\.tsx\?t=/, "above the boundary nothing is stamped");
    console.log("HMR-DEP-UPDATE E2E PASSED");
  } finally {
    await browser.close();
  }
} catch (err) {
  failed = true;
  console.error("HMR-DEP-UPDATE E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
