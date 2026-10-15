// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// An env-file edit restarts the dev server and the open page reloads into the
// restarted server. Everything else about the HMR client is covered by the
// in-process dev tests; this script keeps only the real restart.
// The fixture lives under playground/ so react resolves from its node_modules.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

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
w(
  "src/main.tsx",
  `import { createRoot } from "react-dom/client";\nimport { App } from "./App";\ncreateRoot(document.getElementById("root")!).render(<App />);\n`,
);
w("src/App.tsx", `export function App() {\n  return <h1>v1</h1>;\n}\n`);
w(
  "index.html",
  `<!doctype html><html><head><title>t</title></head><body><div id="root"></div><script type="module" src="/src/main.tsx"></script></body></html>`,
);

let failed = false;
const logFd = fs.openSync(path.join(repo, "playground", ".e2e-hmr-client.log"), "w");
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", logFd, logFd] });
const step = (s) => console.log("step:", s);
try {
  await waitUp(`http://localhost:${PORT}/`);
  const browser = await chromium.launch();
  const page = await browser.newPage();
  try {
    await page.goto(`http://localhost:${PORT}/`, { waitUntil: "networkidle", timeout: 30000 });
    await page.locator("h1", { hasText: "v1" }).waitFor({ timeout: 20000 });
    await page.evaluate(() => {
      window.__NOT_RELOADED = true;
    });

    step("restart");
    // a .env change restarts the server; the page must reload itself. oj
    // answers HTTP before its watcher has registered every watch, so a write
    // can land unseen: rewrite it with a new value until the reload comes.
    let n = 2;
    w(".env", `VITE_FOO=${n++}\n`);
    const reloaded = await settles(() => page.evaluate(() => window.__NOT_RELOADED === undefined).catch(() => false), {
      timeoutMs: 30000,
      touch: () => w(".env", `VITE_FOO=${n++}\n`),
      touchEveryMs: 3000,
    });
    if (!reloaded) throw new Error("a .env edit never reloaded the page");
    await page.locator("h1", { hasText: "v1" }).waitFor({ timeout: 30000 });
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
