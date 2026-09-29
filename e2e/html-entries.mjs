// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// HTML as a build entry, Vite-style: `<link rel="stylesheet">` to a local file
// (relative or root-absolute, Sass included) is compiled, hashed and rewritten;
// an inline `<script type="module">` is bundled as its own entry (relative
// imports resolve against the page) and replaced by a `src`; external links
// stay untouched.

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
const PORT = 5545;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-htmlentries-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "html-entries", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "theme.scss"), `$c: rgb(1, 2, 3);\nh1 { color: $c; }\n`);
fs.writeFileSync(path.join(app, "src", "base.css"), `body { margin: 0; }\n`);
fs.writeFileSync(path.join(app, "src", "x.js"), `export const x = "from-inline";\n`);
fs.writeFileSync(path.join(app, "src", "main.js"), `window.__MAIN = true;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title>\n` +
    `<link rel="stylesheet" href="./src/theme.scss">\n` +
    `<link rel="stylesheet" href="/src/base.css">\n` +
    `<link rel="stylesheet" href="https://cdn.example.test/x.css">\n` +
    `</head><body><h1>hi</h1>\n` +
    `<script type="module">import { x } from "./src/x.js"; document.title = x; window.__INLINE = true;</script>\n` +
    `<script type="module" src="/src/main.js"></script>\n` +
    `</body></html>`,
);

let failed = false;
let srv = null;
try {
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const dist = path.join(app, "dist");
  const html = fs.readFileSync(path.join(dist, "index.html"), "utf8");
  const cssLinks = [...html.matchAll(/<link rel="stylesheet" href="([^"]+)"/g)].map((m) => m[1]);
  assert.ok(
    cssLinks.some((h) => /^\/assets\/theme-[0-9a-f]+\.css$/.test(h)),
    `scss link hashed: ${cssLinks}`,
  );
  assert.ok(
    cssLinks.some((h) => /^\/assets\/base-[0-9a-f]+\.css$/.test(h)),
    `css link hashed: ${cssLinks}`,
  );
  assert.ok(cssLinks.includes("https://cdn.example.test/x.css"), `external link untouched: ${cssLinks}`);
  assert.doesNotMatch(html, /src\/theme\.scss|\/src\/base\.css/, "source paths left in the built html");
  const themeCss = fs.readFileSync(
    path.join(
      dist,
      cssLinks.find((h) => h.includes("theme-")),
    ),
    "utf8",
  );
  assert.match(themeCss, /#010203|rgb\(1,\s*2,\s*3\)/, `scss compiled:\n${themeCss}`);
  assert.doesNotMatch(themeCss, /\$c/, "sass variable shipped");
  const scripts = [...html.matchAll(/<script type="module" src="([^"]+)"(?: crossorigin)?><\/script>/g)].map(
    (m) => m[1],
  );
  assert.equal(scripts.length, 2, `inline + main scripts externalized: ${scripts}`);
  assert.ok(
    scripts.every((s) => /^\/assets\/.+\.js$/.test(s)),
    `both point at hashed chunks: ${scripts}`,
  );
  assert.doesNotMatch(html, /import \{ x \}|@oj-inline/, "inline body or placeholder left in html");
  const inlineChunk = scripts
    .map((s) => fs.readFileSync(path.join(dist, s), "utf8"))
    .find((c) => c.includes("from-inline"));
  assert.ok(inlineChunk, "inline script bundled with its relative import");

  srv = spawn(oj, ["preview", app, "--port", String(PORT)], { stdio: "ignore" });
  await waitUp(`http://localhost:${PORT}/`);
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${PORT}/`, { timeout: 30000 });
    await page.waitForFunction(() => window.__INLINE === true && window.__MAIN === true, { timeout: 20000 });
    assert.equal(await page.title(), "from-inline", "inline module ran with its import");
    const color = await page.evaluate(() => getComputedStyle(document.querySelector("h1")).color);
    assert.equal(color, "rgb(1, 2, 3)", "linked scss applied");
    const margin = await page.evaluate(() => getComputedStyle(document.body).margin);
    assert.equal(margin, "0px", "linked css applied");
    assert.equal(errors.length, 0, `page errors: ${errors.join("|")}`);
  } finally {
    await browser.close();
  }
  console.log("HTML-ENTRIES E2E PASSED");
} catch (err) {
  failed = true;
  console.error("HTML-ENTRIES E2E FAILED:", err.message);
} finally {
  if (srv) srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
