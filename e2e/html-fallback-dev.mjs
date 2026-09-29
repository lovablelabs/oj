// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Vite's dev htmlFallback: `/about` serves about.html, `/nested/` serves
// nested/index.html, an unmatched navigation falls back to the root index.html
// only for appType "spa" (the default); "mpa" answers 404 there and "custom"
// serves no html of its own.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const port = Number(process.env.OJ_E2E_PORT || 5490);

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const page = (title) => `<!doctype html><html><head><title>${title}</title></head><body><h1>${title}</h1></body></html>`;

function makeApp(config) {
  const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-htmlfb-"));
  fs.mkdirSync(path.join(app, "nested"), { recursive: true });
  fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "htmlfb-app", version: "1.0.0" }));
  fs.writeFileSync(path.join(app, "index.html"), page("root"));
  fs.writeFileSync(path.join(app, "about.html"), page("about"));
  fs.writeFileSync(path.join(app, "nested", "index.html"), page("nested"));
  if (config) fs.writeFileSync(path.join(app, "vite.config.mjs"), config);
  return app;
}

const html = { Accept: "text/html,application/xhtml+xml,*/*;q=0.8" };
async function get(pathname, headers = html) {
  const res = await fetch(`http://localhost:${port}${pathname}`, { headers });
  return { status: res.status, body: await res.text() };
}

async function withServer(app, fn) {
  const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
  try {
    await waitUp(`http://localhost:${port}/`, { init: { headers: html }, until: () => true });
    await fn();
  } finally {
    srv.kill("SIGKILL");
    await sleep(300);
    fs.rmSync(app, { recursive: true, force: true });
  }
}

let failed = false;
try {
  await withServer(makeApp(null), async () => {
    assert.match((await get("/about")).body, /<h1>about<\/h1>/, "/about serves about.html");
    assert.match((await get("/nested/")).body, /<h1>nested<\/h1>/, "/nested/ serves nested/index.html");
    assert.match((await get("/about.html")).body, /<h1>about<\/h1>/, "explicit html still served");
    assert.match((await get("/missing/route")).body, /<h1>root<\/h1>/, "spa: unmatched navigation gets index.html");
    assert.match((await get("/about")).body, /\/@oj\/client\.js/, "fallback pages get the dev client injected");
    const json = await get("/about", { Accept: "application/json" });
    assert.doesNotMatch(json.body, /<h1>about<\/h1>/, "a non-html Accept is not rewritten to about.html");
  });
  console.log("spa: ok");

  await withServer(makeApp(`export default { appType: "mpa" };\n`), async () => {
    assert.match((await get("/about")).body, /<h1>about<\/h1>/, "mpa: /about serves about.html");
    assert.match((await get("/nested/")).body, /<h1>nested<\/h1>/, "mpa: /nested/ serves nested/index.html");
    assert.equal((await get("/missing/route")).status, 404, "mpa: no index.html fallback");
  });
  console.log("mpa: ok");

  await withServer(makeApp(`export default { appType: "custom" };\n`), async () => {
    assert.equal((await get("/about")).status, 404, "custom: no html fallback");
    assert.equal((await get("/missing/route")).status, 404, "custom: no spa fallback");
  });
  console.log("custom: ok");
  console.log("HTML-FALLBACK-DEV E2E PASSED");
} catch (err) {
  failed = true;
  console.error("HTML-FALLBACK-DEV E2E FAILED:", err.message);
}
process.exit(failed ? 1 : 0);
