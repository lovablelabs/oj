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

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-rawtarget-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "rt-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "note.txt"), "hello-raw-content");
fs.writeFileSync(
  path.join(app, "src", "dot.png"),
  Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==", "base64"),
);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import raw from "./note.txt?raw";\nimport inl from "./dot.png?inline";\n` +
    `const get = (o) => o?.deep?.value ?? "fallback";\n` +
    `window.__RAW = raw; window.__INL = inl; window.__G = get({});\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

const buildEntry = () => {
  fs.rmSync(path.join(app, "dist"), { recursive: true, force: true });
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const f = fs.readdirSync(path.join(app, "dist", "assets")).find((x) => x.startsWith("main-") && x.endsWith(".js"));
  return fs.readFileSync(path.join(app, "dist", "assets", f), "utf8");
};

let failed = false;
try {
  // default target (esnext): ?raw + ?inline applied, optional chaining preserved
  let code = buildEntry();
  assert.match(code, /hello-raw-content/, "?raw content not inlined into build");
  assert.match(code, /data:image\/png;base64,/, "?inline not a base64 data uri in build");
  assert.ok(code.includes("?."), "esnext build should keep optional chaining");

  // target es2015: ES2020 syntax lowered, assets still applied
  fs.writeFileSync(path.join(app, "oj.config.json"), JSON.stringify({ build: { target: "es2015" } }));
  code = buildEntry();
  assert.match(code, /hello-raw-content/, "?raw survives target downleveling");
  assert.match(code, /data:image\/png;base64,/, "?inline survives target downleveling");
  assert.ok(!code.includes("?."), "es2015 target must lower optional chaining");

  // runtime check on the downleveled build
  const srv = spawn(oj, ["preview", app, "--port", "5381"], { stdio: "ignore" });
  await waitUp("http://localhost:5381/");
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto("http://localhost:5381/", { timeout: 30000 });
    await page.waitForFunction(() => window.__RAW !== undefined, { timeout: 10000 });
    const r = await page.evaluate(() => ({ raw: window.__RAW, inl: window.__INL, g: window.__G }));
    assert.equal(r.raw, "hello-raw-content");
    assert.match(r.inl, /^data:image\/png;base64,/);
    assert.equal(r.g, "fallback", "downleveled optional-chaining/nullish must still evaluate");
    assert.equal(errors.length, 0, `page errors: ${errors.join("|")}`);
  } finally {
    await browser.close();
    srv.kill("SIGKILL");
  }

  console.log("BUILD-TARGET-RAW-INLINE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("BUILD-TARGET-RAW-INLINE E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
