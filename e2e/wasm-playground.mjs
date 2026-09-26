// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// The www wasm playground, end to end in a real browser: the oj_wasm build
// boots, the preview renders through the import map of blob urls, CSS Modules
// scope, edits rebuild live, a broken import raises the error strip while the
// last good preview stays up, and burst typing never blanks the front pane
// (the preview is double-buffered).
//
// Skips itself when its prerequisites are missing: playwright (installed under
// e2e/), the www dependencies, and a wasm bundle (built here when wasm-pack is
// on the PATH, reused from www/public/oj-wasm when not).

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const www = path.join(repo, "www");
const oj = path.join(repo, "target", "debug", "oj");
// A free port from the OS, so nothing unrelated gets killed and parallel runs
// don't collide.
const PORT = await new Promise((resolve) => {
  const probe = net.createServer();
  probe.listen(0, () => {
    const { port } = probe.address();
    probe.close(() => resolve(port));
  });
});

let chromium;
try {
  ({ chromium } = createRequire(path.join(here, "x.js"))("playwright"));
} catch {
  console.log("SKIP wasm-playground: playwright not installed under e2e/");
  process.exit(0);
}
if (!fs.existsSync(path.join(www, "node_modules"))) {
  console.log("SKIP wasm-playground: www dependencies not installed (npm ci --prefix www)");
  process.exit(0);
}
// Rebuild the wasm whenever wasm-pack is available so the browser exercises
// the CURRENT crates/oj_wasm sources; a stale prebuilt bundle only stands in
// when wasm-pack is missing entirely.
let hasWasmPack = true;
try {
  execSync("wasm-pack --version", { stdio: "ignore" });
} catch {
  hasWasmPack = false;
}
if (hasWasmPack) {
  // The --dev profile: functionally identical, a fraction of the build time.
  execSync("npm run build:wasm:dev", { cwd: www, stdio: "inherit" });
} else if (!fs.existsSync(path.join(www, "public", "oj-wasm", "oj_wasm.js"))) {
  console.log("SKIP wasm-playground: no wasm bundle and no wasm-pack on PATH");
  process.exit(0);
} else {
  console.log("wasm-playground: wasm-pack missing, testing the prebuilt bundle in www/public/oj-wasm");
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// The server log is kept so a startup failure carries its own explanation:
// a loaded CI runner is exactly where startup misbehaves, and a silent kill
// leaves nothing to diagnose from. Ready = the port answers, within a
// wall-clock deadline; a dead server fails immediately instead of waiting
// out the clock.
let serverLog = "";
const server = spawn(oj, ["dev", www, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
server.stdout.on("data", (d) => { serverLog += d; });
server.stderr.on("data", (d) => { serverLog += d; });
let up = false;
const deadline = Date.now() + 60_000;
while (!up && Date.now() < deadline && server.exitCode === null) {
  try {
    up = (await fetch(`http://localhost:${PORT}/`)).ok;
  } catch {}
  if (!up) await sleep(250);
}
if (!up) {
  server.kill("SIGKILL");
  throw new Error(`dev server did not start:\n${serverLog}`);
}

const bad = [];
const browser = await chromium.launch();
try {
  const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
  page.on("pageerror", (e) => console.log("  page error:", e.message));
  await page.goto(`http://localhost:${PORT}/`, { waitUntil: "domcontentloaded" });

  // The front pane changes identity on every double-buffer swap; frameLocator
  // re-resolves the selector on each use.
  const front = () => page.frameLocator('iframe.play__frame[data-front="true"]');
  const selectAll = process.platform === "darwin" ? "Meta+a" : "Control+a";

  // boot: wasm + editor
  await page.waitForSelector(".play__cm .cm-editor", { timeout: 60000 }).catch(() => bad.push("editor never appeared"));

  // first render through blob import map + esm.sh react
  let h1 = null;
  for (let i = 0; i < 120; i++) {
    h1 = await front().locator("h1").textContent({ timeout: 1000 }).catch(() => null);
    if (h1 && h1.includes("juice stand")) break;
    await sleep(500);
  }
  if (!h1 || !h1.includes("juice stand")) bad.push(`preview h1: ${h1}`);

  // css module class applied; state updates on click
  const cls = await front().locator("button").getAttribute("class");
  if (!cls || !cls.includes("squeeze")) bad.push(`button class: ${cls}`);
  await front().locator("button").click();
  await front().locator("button").click();
  const tally = await front().locator("p").last().textContent().catch(() => null);
  if (!tally?.includes("2 glasses")) bad.push(`tally after 2 clicks: ${tally}`);

  // linked stylesheet inlined into the document
  const bg = await front().locator("body").evaluate((el) => getComputedStyle(el).backgroundColor);
  if (bg !== "rgb(255, 248, 240)") bad.push(`body background: ${bg}`);

  // live edit round trip (insertText: keyboard.type fights cm autoclose)
  await page.click(".play__tab >> text=global.css");
  await page.click(".play__cm .cm-content");
  await page.keyboard.press(selectAll);
  await page.keyboard.insertText("body { background: rgb(10, 20, 30); color: white } main { max-width: 26rem; margin: 3rem auto }");
  let newBg = null;
  for (let i = 0; i < 40; i++) {
    await sleep(500);
    newBg = await front().locator("body").evaluate((el) => getComputedStyle(el).backgroundColor).catch(() => null);
    if (newBg === "rgb(10, 20, 30)") break;
  }
  if (newBg !== "rgb(10, 20, 30)") bad.push(`background after edit: ${newBg}`);

  // a missing import raises the strip and keeps the last good preview
  await page.click(".play__tab >> text=App.tsx");
  await page.click(".play__cm .cm-content");
  await page.keyboard.press(selectAll);
  await page.keyboard.insertText("import Broken from './nope';\nexport default function App(){ return <Broken /> }");
  await page.waitForSelector(".play__errors", { timeout: 10000 }).catch(() => bad.push("error strip never appeared"));
  const errText = await page.locator(".play__error").first().textContent().catch(() => "");
  if (!errText.includes("nope")) bad.push(`error text: ${errText}`);
  const staleH1 = await front().locator("h1").textContent().catch(() => null);
  if (!staleH1 || !staleH1.includes("juice stand")) bad.push(`preview lost during error: ${staleH1}`);

  // recovery clears the strip and updates the preview
  await page.keyboard.press(selectAll);
  await page.keyboard.insertText("export default function App(){ return <h1>fixed!</h1> }");
  let fixed = null;
  for (let i = 0; i < 40; i++) {
    await sleep(500);
    fixed = await front().locator("h1").textContent().catch(() => null);
    if (fixed === "fixed!") break;
  }
  if (fixed !== "fixed!") bad.push(`h1 after fix: ${fixed}`);
  if ((await page.locator(".play__errors").count()) !== 0) bad.push("error strip still visible after fix");

  // burst typing: the visible pane must never be empty between swaps
  await page.keyboard.press(selectAll);
  await page.keyboard.insertText("export default function App(){ return <h1>burst 0</h1> }");
  let blanks = 0;
  for (let i = 1; i <= 8; i++) {
    await front().locator("h1").waitFor({ timeout: 3000 }).catch(() => {});
    await page.keyboard.press(selectAll);
    await page.keyboard.insertText(`export default function App(){ return <h1>burst ${i}</h1> }`);
    const seen = await front().locator("h1").count().catch(() => 0);
    if (seen === 0) blanks++;
    await sleep(150);
  }
  let last = null;
  for (let i = 0; i < 40; i++) {
    await sleep(500);
    last = await front().locator("h1").textContent().catch(() => null);
    if (last === "burst 8") break;
  }
  if (last !== "burst 8") bad.push(`h1 after burst: ${last}`);
  if (blanks > 0) bad.push(`front pane was empty ${blanks} time(s) during burst typing`);
} finally {
  await browser.close();
  server.kill("SIGKILL");
}

if (bad.length) {
  console.log("FAIL wasm-playground\n" + bad.map((b) => "  - " + b).join("\n"));
  process.exit(1);
}
console.log("PASS wasm-playground: boot, render, css modules, clicks, inline css, live edit, error strip, recovery, burst typing");
