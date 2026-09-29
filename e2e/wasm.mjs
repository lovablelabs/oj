// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

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

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-wasm-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "wasm-app", version: "1.0.0" }));
// (module (func (export "add") (param i32 i32) (result i32) local.get 0 local.get 1 i32.add))
const wasm = Buffer.from([0, 0x61, 0x73, 0x6d, 1, 0, 0, 0, 1, 7, 1, 0x60, 2, 0x7f, 0x7f, 1, 0x7f, 3, 2, 1, 0, 7, 7, 1, 3, 0x61, 0x64, 0x64, 0, 0, 10, 9, 1, 7, 0, 0x20, 0, 0x20, 1, 0x6a, 0x0b]);
fs.writeFileSync(path.join(app, "src", "add.wasm"), wasm);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import init from "./add.wasm?init";\ninit().then((instance) => { window.__SUM = instance.exports.add(2, 3); });\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

async function inBrowser(port) {
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${port}/`, { timeout: 30000 });
    const sum = await page
      .waitForFunction(() => window.__SUM !== undefined, { timeout: 10000 })
      .then(() => page.evaluate(() => window.__SUM));
    if (sum !== 5) throw new Error(`add(2,3)=${sum}`);
    if (errors.length) throw new Error(`page errors ${errors.join("|")}`);
  } finally {
    await browser.close();
  }
}

async function serve(cmd, port) {
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  const srv = spawn(oj, [cmd, app, "--port", String(port)], { stdio: "ignore" });
  await waitUp(`http://localhost:${port}/`);
  return srv;
}

let failed = false;
try {
  let s = await serve("dev", 5317);
  await inBrowser(5317);
  s.kill("SIGKILL");
  console.log("[non-bundle dev] OK");

  fs.rmSync(path.join(app, "dist"), { recursive: true, force: true });
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  if (!fs.readdirSync(path.join(app, "dist", "assets")).some((f) => f.endsWith(".wasm")))
    throw new Error("prod build did not emit the wasm asset");
  s = await serve("preview", 5318);
  await inBrowser(5318);
  s.kill("SIGKILL");
  console.log("[prod build + preview] OK");

  console.log("WASM E2E PASSED");
} catch (err) {
  failed = true;
  console.error("WASM E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
