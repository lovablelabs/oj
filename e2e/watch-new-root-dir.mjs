// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Top-level entries created AFTER boot must be watched: the per-entry root
// watches only cover what read_dir saw at startup, so a new root-level file
// or a freshly mkdir'd top-level directory (an agent adding a shared/ tree to
// a running server) served fine but never produced watcher events -- edits
// there kept the old module until restart. Vite's chokidar watches the root
// itself and picks up new children; this drives both shapes end to end.
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
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const PORT = 5499;

if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-watch-newdir-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "watch-newdir", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "main.js"), `document.title = "v1"; window.__READY = true;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
let browser;
try {
  await waitUp(`http://localhost:${PORT}/`, { proc: srv });
  browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  await page.goto(`http://localhost:${PORT}/`, { timeout: 30000 });
  await page.waitForFunction(() => window.__READY === true, { timeout: 10000 });

  // A root-level file created after boot: the import lands via the (always
  // watched) src/main.js edit; the edit AFTER that only fires if the root
  // watch reports its new children.
  fs.writeFileSync(path.join(app, "banner.js"), `export const banner = "B1";\n`);
  fs.writeFileSync(
    path.join(app, "src", "main.js"),
    `import { banner } from "../banner.js";\ndocument.title = banner; window.__READY = true;\n`,
  );
  await page.waitForFunction(() => document.title === "B1", { timeout: 10000 });
  fs.writeFileSync(path.join(app, "banner.js"), `export const banner = "B2";\n`);
  await page.waitForFunction(() => document.title === "B2", { timeout: 10000 });
  console.log("new root-level file: edit after creation reloads");

  // A top-level directory created after boot, then an edit inside it.
  fs.mkdirSync(path.join(app, "lib"));
  fs.writeFileSync(path.join(app, "lib", "dep.js"), `export const label = "L1";\n`);
  fs.writeFileSync(
    path.join(app, "src", "main.js"),
    `import { banner } from "../banner.js";\nimport { label } from "../lib/dep.js";\ndocument.title = banner + "-" + label; window.__READY = true;\n`,
  );
  await page.waitForFunction(() => document.title === "B2-L1", { timeout: 10000 });
  fs.writeFileSync(path.join(app, "lib", "dep.js"), `export const label = "L2";\n`);
  await page.waitForFunction(() => document.title === "B2-L2", { timeout: 10000 });
  console.log("new top-level dir: edit after mkdir reloads");

  // A staged tree RENAMED into the root (same filesystem, so a true rename):
  // notify reports it as Modify(Name), not Create, and it must be adopted the
  // same way.
  const staging = fs.mkdtempSync(path.join(os.tmpdir(), "oj-watch-stage-"));
  fs.mkdirSync(path.join(staging, "pkg"));
  fs.writeFileSync(path.join(staging, "pkg", "mod.js"), `export const tag = "P1";\n`);
  fs.renameSync(path.join(staging, "pkg"), path.join(app, "pkg"));
  fs.writeFileSync(
    path.join(app, "src", "main.js"),
    `import { banner } from "../banner.js";\nimport { label } from "../lib/dep.js";\nimport { tag } from "../pkg/mod.js";\ndocument.title = banner + "-" + label + "-" + tag; window.__READY = true;\n`,
  );
  await page.waitForFunction(() => document.title === "B2-L2-P1", { timeout: 10000 });
  fs.writeFileSync(path.join(app, "pkg", "mod.js"), `export const tag = "P2";\n`);
  await page.waitForFunction(() => document.title === "B2-L2-P2", { timeout: 10000 });
  fs.rmSync(staging, { recursive: true, force: true });
  console.log("renamed-in top-level dir: edit after rename reloads");

  assert.equal(errors.length, 0, `page errors: ${errors.join("|")}`);
  console.log("WATCH-NEW-ROOT-DIR E2E PASSED");
} catch (err) {
  failed = true;
  console.error("WATCH-NEW-ROOT-DIR E2E FAILED:", err.message);
} finally {
  if (browser) await browser.close();
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
