// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-qassets-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "qassets-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "pic.svg"), `<svg xmlns="http://www.w3.org/2000/svg"></svg>`);
fs.writeFileSync(path.join(app, "src", "note.txt"), `hello-raw-content`);
fs.writeFileSync(path.join(app, "src", "data.json"), `{"k":42}`);
fs.writeFileSync(
  path.join(app, "src", "add.wasm"),
  Buffer.from([0, 0x61, 0x73, 0x6d, 1, 0, 0, 0, 1, 7, 1, 0x60, 2, 0x7f, 0x7f, 1, 0x7f, 3, 2, 1, 0, 7, 7, 1, 3, 0x61, 0x64, 0x64, 0, 0, 10, 9, 1, 7, 0, 0x20, 0, 0x20, 1, 0x6a, 0x0b]),
);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import picUrl from "./pic.svg?url";\n` +
    `import raw from "./note.txt?raw";\n` +
    `import inlineData from "./data.json?inline";\n` +
    `import init from "./add.wasm?init";\n` +
    `window.__PICURL = picUrl;\n` +
    `window.__RAW = raw;\n` +
    `window.__INLINE = inlineData.startsWith("data:");\n` +
    `init().then((i) => { window.__SUM = i.exports.add(4, 5); });\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

async function check(port) {
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  const args = ["dev", app, "--port", String(port)];
  const srv = spawn(oj, args, { stdio: "ignore" });
  for (let i = 0; i < 80; i++) { try { if ((await fetch(`http://localhost:${port}/`)).ok) break; } catch {} await sleep(200); }
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${port}/`, { timeout: 30000 });
    await page.waitForFunction(() => window.__SUM !== undefined, { timeout: 10000 });
    const r = await page.evaluate(() => ({ url: window.__PICURL, raw: window.__RAW, inline: window.__INLINE, sum: window.__SUM }));
    const bad = [];
    if (r.url !== "/src/pic.svg") bad.push(`?url ${r.url}`);
    if (r.raw !== "hello-raw-content") bad.push(`?raw ${r.raw}`);
    if (r.inline !== true) bad.push(`?inline ${r.inline}`);
    if (r.sum !== 9) bad.push(`?init add=${r.sum}`);
    if (errors.length) bad.push(`errors ${errors.join("|")}`);
    if (bad.length) throw new Error(`[dev] ${bad.join("; ")}`);
    console.log(`[dev] url/raw/inline/init all OK`);
  } finally {
    await browser.close();
    srv.kill("SIGKILL");
  }
}

let failed = false;
try {
  await check(5323);
  console.log("QUERY-ASSETS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("QUERY-ASSETS E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
