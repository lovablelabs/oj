// SPDX-License-Identifier: MIT
// SolidJS support e2e (plain Vite + vite-plugin-solid app, no TanStack Start).
// Shape mirrors e2e/svelte.mjs: dev render + reactivity + HMR hot-swap, then a
// production preview render. Run: `node e2e/solid.mjs`.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

execSync("cargo build -p adaptive-oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-solid-"));
fs.writeFileSync(
  path.join(app, "package.json"),
  JSON.stringify({ name: "solid-app", version: "1.0.0", type: "module" }),
);
try {
  execSync("npm install solid-js vite vite-plugin-solid --no-audit --no-fund --loglevel=error", {
    cwd: app,
    stdio: "ignore",
  });
} catch {
  console.log("SKIP solid: could not install deps (offline?)");
  fs.rmSync(app, { recursive: true, force: true });
  process.exit(0);
}

fs.mkdirSync(path.join(app, "src"), { recursive: true });
const appTsx = (label) =>
  `import { createSignal } from "solid-js";\nexport default function Counter() {\n  const [count, setCount] = createSignal(0);\n  return <button id="btn" onClick={() => setCount((c) => c + 1)}>${label}: {count()}</button>;\n}\n`;
fs.writeFileSync(path.join(app, "src", "App.tsx"), appTsx("count"));
fs.writeFileSync(
  path.join(app, "src", "main.tsx"),
  `import { render } from "solid-js/web";\nimport Counter from "./App.tsx";\nrender(() => <Counter />, document.getElementById("app"));\n`,
);
fs.writeFileSync(
  path.join(app, "vite.config.mjs"),
  `import solid from "vite-plugin-solid";\nexport default { plugins: [solid()], server: { port: 5441 } };\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><div id="app"></div><script type="module" src="/src/main.tsx"></script></body></html>`,
);

async function renders(page) {
  const btn = await page.waitForSelector("#btn", { timeout: 10000 });
  if ((await page.evaluate((e) => e.textContent, btn)) !== "count: 0") throw new Error("initial render wrong");
  await btn.click();
  if ((await page.evaluate((e) => e.textContent, btn)) !== "count: 1") throw new Error("reactivity broken");
  return btn;
}

async function mode(label, args, port, build, hmr) {
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  fs.writeFileSync(path.join(app, "src", "App.tsx"), appTsx("count"));
  if (build) {
    fs.rmSync(path.join(app, "dist"), { recursive: true, force: true });
    execSync(`${oj} build ${app}`, { stdio: "ignore" });
  }
  const srv = spawn(oj, args, { stdio: "ignore" });
  await waitUp(`http://localhost:${port}/`);
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${port}/`, { timeout: 30000 });
    // Solid output must never carry React Refresh registrations.
    const body = await page.content();
    if (body.includes("$RefreshReg$")) throw new Error("react refresh leaked into solid output");
    await renders(page);
    if (hmr) {
      await page.evaluate(() => (window.__SURVIVE = "yes"));
      fs.writeFileSync(path.join(app, "src", "App.tsx"), appTsx("clicks"));
      await page.waitForFunction(() => document.getElementById("btn")?.textContent?.startsWith("clicks:"), null, {
        timeout: 10000,
      });
      const survived = await page.evaluate(() => window.__SURVIVE);
      if (survived !== "yes") throw new Error("HMR did a full reload instead of a hot swap");
    }
    if (errors.length) throw new Error(`page errors: ${errors.join("|")}`);
    console.log(`[${label}] solid OK${hmr ? " (+hot swap, no reload)" : ""}`);
  } finally {
    await browser.close();
    srv.kill("SIGKILL");
    await sleep(300);
  }
}

let failed = false;
try {
  await mode("dev", ["dev", app, "--port", "5441"], 5441, false, true);
  await mode("prod", ["preview", app, "--port", "5443"], 5443, true, false);
  console.log("SOLID E2E PASSED");
} catch (err) {
  failed = true;
  console.error("SOLID E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
