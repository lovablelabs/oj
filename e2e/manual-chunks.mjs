// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

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

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-mc-"));
const pkg = (name, body) => {
  const dir = path.join(app, "node_modules", name);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(
    path.join(dir, "package.json"),
    JSON.stringify({ name, version: "1.0.0", module: "index.js", main: "index.js" }),
  );
  fs.writeFileSync(path.join(dir, "index.js"), body);
};
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "mc-app", version: "1.0.0" }));
pkg("dep-a", `export const a = "DEP_A_MARKER_" + 42;`);
pkg("dep-b", `export const b = "DEP_B_MARKER_" + 7;`);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import { a } from "dep-a";\nimport { b } from "dep-b";\nwindow.__V = a + b;\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "oj.config.json"),
  JSON.stringify({ build: { rolldownOptions: { output: { manualChunks: { vendor: ["dep-a", "dep-b"] } } } } }),
);

let failed = false;
try {
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const dir = path.join(app, "dist", "assets");
  const files = fs.readdirSync(dir);
  const vendor = files.find((f) => f.startsWith("vendor-") && f.endsWith(".js"));
  const main = files.find((f) => f.startsWith("main-") && f.endsWith(".js"));
  assert.ok(vendor, `no vendor chunk emitted: ${files.join(",")}`);
  const vendorCode = fs.readFileSync(path.join(dir, vendor), "utf8");
  assert.match(vendorCode, /DEP_A_MARKER/, "vendor chunk missing dep-a");
  assert.match(vendorCode, /DEP_B_MARKER/, "vendor chunk missing dep-b");
  const mainCode = fs.readFileSync(path.join(dir, main), "utf8");
  assert.ok(!/DEP_A_MARKER|DEP_B_MARKER/.test(mainCode), "deps must be split out of the main chunk");

  const srv = spawn(oj, ["preview", app, "--port", "5382"], { stdio: "ignore" });
  await waitUp("http://localhost:5382/");
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto("http://localhost:5382/", { timeout: 30000 });
    const v = await page
      .waitForFunction(() => window.__V, { timeout: 10000 })
      .then(() => page.evaluate(() => window.__V));
    assert.equal(v, "DEP_A_MARKER_42DEP_B_MARKER_7", "split chunks did not link at runtime");
    assert.equal(errors.length, 0, `page errors: ${errors.join("|")}`);
  } finally {
    await browser.close();
    srv.kill("SIGKILL");
  }

  console.log("MANUAL-CHUNKS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("MANUAL-CHUNKS E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
