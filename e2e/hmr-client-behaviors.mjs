// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite-parity behaviours of the HMR client and server decisions:
//  - an edit inside an import cycle below a boundary hot-updates (no reload);
//  - an edit to a boundary the page never loaded is ignored (no reload);
//  - frames are Vite's UpdatePayload / ErrorPayload shapes;
//  - after the server restarts (config change) the page reloads.
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
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const PORT = 5492;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = path.join(repo, "playground", ".e2e-hmr-client");
fs.rmSync(app, { recursive: true, force: true });
fs.mkdirSync(path.join(app, "src"), { recursive: true });
const w = (rel, s) => fs.writeFileSync(path.join(app, rel), s);
w("package.json", JSON.stringify({ name: "hmr-client", version: "1.0.0" }));
w(".env", "VITE_FOO=1\n");
w("style.css", `h1 { color: rgb(1, 2, 3); }\n`);
w(
  "src/main.tsx",
  `import { createRoot } from "react-dom/client";\nimport { App } from "./App";\ncreateRoot(document.getElementById("root")!).render(<App />);\n`,
);
// a <-> b cycle (barrel style), both below App
w("src/a.ts", `import { fromB } from "./b";\nexport const label = "v1";\nexport const viaB = () => fromB();\n`);
w("src/b.ts", `import { label } from "./a";\nexport const fromB = () => label + "!";\n`);
w("src/Lazy.tsx", `export default function Lazy() { return <p>lazy</p>; }\n`);
w(
  "src/App.tsx",
  `import { useState } from "react";\nimport { viaB } from "./a";\n` +
    `export function App() {\n  const [n, setN] = useState(0);\n` +
    `  // never called: Lazy stays unloaded in the browser but is in the server graph\n` +
    `  (window as any).__loadLazy = () => import("./Lazy");\n` +
    `  return (<div><h1>{viaB()}</h1><button onClick={() => setN(n + 1)}>Clicks: {n}</button></div>);\n}\n`,
);
w(
  "index.html",
  `<!doctype html><html><head><title>t</title><link rel="stylesheet" href="/style.css"></head><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>`,
);

let failed = false;
const logFd = fs.openSync(path.join(repo, "playground", ".e2e-hmr-client.log"), "w");
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", logFd, logFd] });
const step = (s) => console.log("step:", s);
try {
  await waitUp(`http://localhost:${PORT}/`);
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${PORT}/`, { waitUntil: "networkidle", timeout: 30000 });
    await page.locator("h1", { hasText: "v1!" }).waitFor({ timeout: 20000 });
    await page.evaluate(async () => {
      window.__frames = [];
      // A browser upgrade must carry the per-process token the served client
      // dials with (Vite's webSocketToken); read it off the client source.
      const client = await (await fetch("/@oj/client.js")).text();
      const token = /const wsToken = "([0-9a-f]+)";/.exec(client)[1];
      const ws = new WebSocket("ws://" + location.host + "/__ws?token=" + token);
      ws.onmessage = (e) => window.__frames.push(JSON.parse(e.data));
      window.__NOT_RELOADED = true;
    });
    const btn = page.locator("button");
    await btn.click();

    step("cycle edit");
    // 1. edit inside the cycle: hot update through App, no reload
    w("src/b.ts", `import { label } from "./a";\nexport const fromB = () => label + "?";\n`);
    await page.locator("h1", { hasText: "v1?" }).waitFor({ timeout: 20000 });
    assert.equal(await page.evaluate(() => window.__NOT_RELOADED), true, "cycle edit reloaded the page");
    assert.match(await btn.textContent(), /Clicks: 1/, "state lost on cycle edit");

    step("unloaded boundary");
    // 2. edit an unloaded boundary: ignored, no reload
    w("src/Lazy.tsx", `export default function Lazy() { return <p>lazy2</p>; }\n`);
    await sleep(1500);
    assert.equal(await page.evaluate(() => window.__NOT_RELOADED), true, "unloaded module edit reloaded the page");

    step("frames");
    // 3. frame shapes: css-update inside an UpdatePayload, ErrorPayload with err
    w("style.css", `h1 { color: rgb(4, 5, 6); }\n`);
    await page.waitForFunction(() => getComputedStyle(document.querySelector("h1")).color === "rgb(4, 5, 6)", {
      timeout: 20000,
    });
    const css = await page.evaluate(() =>
      window.__frames.find((f) => f.type === "update" && f.updates.some((u) => u.type === "css-update")),
    );
    assert.ok(
      css,
      `no UpdatePayload with a css-update entry: ${JSON.stringify(await page.evaluate(() => window.__frames))}`,
    );
    const entry = css.updates.find((u) => u.type === "css-update");
    assert.equal(entry.path, "/style.css");
    assert.equal(entry.acceptedPath, "/style.css");
    assert.equal(typeof entry.timestamp, "number");
    const js = await page.evaluate(() =>
      window.__frames.find((f) => f.type === "update" && f.updates.some((u) => u.type === "js-update")),
    );
    assert.ok(js && js.updates[0].acceptedPath, "js-update entries carry type and acceptedPath");
    step("error frame");
    w("src/b.ts", `import { label } from "./a";\nexport const fromB = () => label + ;\n`);
    await page.waitForFunction(() => window.__frames.some((f) => f.type === "error"), { timeout: 20000 });
    const err = await page.evaluate(() => window.__frames.find((f) => f.type === "error"));
    assert.ok(
      err.err && typeof err.err.message === "string" && err.err.message.length > 0,
      `ErrorPayload shape: ${JSON.stringify(err)}`,
    );
    assert.equal(err.message, undefined, "legacy top-level message is gone");
    await page.locator('[role="dialog"]').waitFor({ timeout: 10000 });
    w("src/b.ts", `import { label } from "./a";\nexport const fromB = () => label + "#";\n`);
    await page.locator("h1", { hasText: "v1#" }).waitFor({ timeout: 20000 });
    assert.equal(await page.evaluate(() => window.__NOT_RELOADED), true, "error round-trip reloaded the page");

    step("restart");
    // 4. a config/.env change restarts the server; the page must reload itself
    await sleep(500);
    w(".env", "VITE_FOO=2\n");
    await page.waitForFunction(() => window.__NOT_RELOADED === undefined, { timeout: 30000 });
    await page.locator("h1", { hasText: "v1#" }).waitFor({ timeout: 30000 });
    console.log("HMR-CLIENT-BEHAVIORS E2E PASSED");
  } finally {
    await browser.close();
  }
} catch (err) {
  failed = true;
  console.error("HMR-CLIENT-BEHAVIORS E2E FAILED:", err.message);
  try {
    console.error(
      fs
        .readFileSync(path.join(repo, "playground", ".e2e-hmr-client.log"), "utf8")
        .split("\n")
        .slice(-25)
        .join("\n"),
    );
  } catch {}
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
  fs.rmSync(path.join(repo, "playground", ".e2e-hmr-client.log"), { force: true });
}
process.exit(failed ? 1 : 0);
