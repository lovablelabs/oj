// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Worker imports in the production build, every Vite spelling: `?worker`,
// `?worker&inline` (the worker code ships inside the chunk), `?worker&url`, and
// `new Worker(new URL("./w.ts", import.meta.url), { type: "module" })` (a worker
// chunk, not a raw `.ts` asset). Plus a template-literal asset url
// `new URL(\`./img/${name}.png\`, import.meta.url)` resolving through the glob.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
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
const PORT = 5543;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-workerbuild-"));
fs.mkdirSync(path.join(app, "src", "img"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "worker-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "img", "pic.png"), Buffer.alloc(6000, 3));
fs.writeFileSync(path.join(app, "src", "img", "other.png"), Buffer.alloc(6000, 4));
fs.writeFileSync(path.join(app, "src", "w-lazy.ts"), `export const suffix: string = "!";\n`);
fs.writeFileSync(
  path.join(app, "src", "w.ts"),
  `const tag: string = "w";\nself.onmessage = async (e) => { const { suffix } = await import("./w-lazy.ts"); self.postMessage(tag + ":" + e.data + suffix); };\n`,
);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import W from "./w.ts?worker";\nimport WI from "./w.ts?worker&inline";\nimport wUrl from "./w.ts?worker&url";\n` +
    `const ask = (w, m) => new Promise((r) => { w.onmessage = (e) => r(e.data); w.postMessage(m); });\n` +
    `const direct = new Worker(new URL("./w.ts", import.meta.url), { type: "module" });\n` +
    `const name = "pic";\nwindow.__IMG = new URL(\`./img/\${name}.png\`, import.meta.url).href;\n` +
    `window.__WURL = wUrl;\n` +
    `Promise.all([ask(W(), "a"), ask(WI(), "b"), ask(direct, "c")]).then((r) => { window.__W = r; });\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

let failed = false;
let srv = null;
try {
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const assets = path.join(app, "dist", "assets");
  const files = fs.readdirSync(assets);
  const main = fs.readFileSync(path.join(assets, files.find((f) => f.startsWith("main-") && f.endsWith(".js"))), "utf8");
  assert.ok(files.some((f) => /^w-[^.]+\.js$/.test(f)), `worker chunk emitted: ${files}`);
  assert.ok(!files.some((f) => f.endsWith(".ts")), `raw .ts shipped as an asset: ${files}`);
  assert.match(main, /self\.onmessage|postMessage/, "inline worker code embedded in the main chunk");
  assert.match(main, /pic-[^.]+\.png/, "template-literal url matched the glob");
  assert.match(main, /other-[^.]+\.png/, "glob map includes every match");
  // A worker chunk (and the chunks only it loads) runs without a document: no
  // __vitePreload wrapping or stylesheet injection lands in it (Vite: isWorker).
  const workerChunk = fs.readFileSync(path.join(assets, files.find((f) => /^w-[^.]+\.js$/.test(f))), "utf8");
  assert.doesNotMatch(workerChunk, /__vitePreload|document\./, "worker chunk is free of document-only helpers");
  assert.match(workerChunk, /import\(/, "the worker keeps its dynamic import");
  const lazyChunk = files.find((f) => /^w-lazy-[^.]+\.js$/.test(f));
  assert.ok(lazyChunk, `worker-only lazy chunk emitted: ${files}`);
  assert.doesNotMatch(fs.readFileSync(path.join(assets, lazyChunk), "utf8"), /document\./, "worker-only chunk has no document access");

  srv = spawn(oj, ["preview", app, "--port", String(PORT)], { stdio: "ignore" });
  await waitUp(`http://localhost:${PORT}/`);
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${PORT}/`, { timeout: 30000 });
    await page.waitForFunction(() => Array.isArray(window.__W), { timeout: 20000 });
    const r = await page.evaluate(() => ({ w: window.__W, img: window.__IMG, wurl: window.__WURL }));
    assert.deepEqual(r.w, ["w:a!", "w:b!", "w:c!"], "all three worker forms answered (through the worker's dynamic import)");
    assert.match(r.img, /\/assets\/pic-[^.]+\.png$/, `template url resolved to the hashed asset: ${r.img}`);
    assert.match(r.wurl, /\/assets\/w-[^.]+\.js$/, `?worker&url is the chunk url: ${r.wurl}`);
    assert.equal(errors.length, 0, `page errors: ${errors.join("|")}`);
  } finally {
    await browser.close();
  }
  console.log("WORKER-BUILD E2E PASSED");
} catch (err) {
  failed = true;
  console.error("WORKER-BUILD E2E FAILED:", err.message);
} finally {
  if (srv) srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
