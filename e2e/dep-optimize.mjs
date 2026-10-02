// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath, pathToFileURL } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const esbuildSrc = path.join(repo, "e2e/fixtures/start-app/node_modules/esbuild");
const port = 5273;

if (!fs.existsSync(esbuildSrc)) {
  console.log("SKIP dep-optimize: esbuild fixture not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-optdep-"));
const nm = path.join(app, "node_modules");
fs.mkdirSync(path.join(nm, "cjs-lib"), { recursive: true });
fs.symlinkSync(esbuildSrc, path.join(nm, "esbuild"));
const esbuildScoped = path.join(repo, "e2e/fixtures/start-app/node_modules/@esbuild");
if (fs.existsSync(esbuildScoped)) fs.symlinkSync(esbuildScoped, path.join(nm, "@esbuild"));

fs.writeFileSync(
  path.join(nm, "cjs-lib", "package.json"),
  JSON.stringify({ name: "cjs-lib", version: "1.0.0", main: "index.js" }),
);
fs.writeFileSync(
  path.join(nm, "cjs-lib", "index.js"),
  `"use strict";\n` +
    `Object.defineProperty(exports, "__esModule", { value: true });\n` +
    `Object.defineProperty(exports, "greet", { enumerable: true, get: function () { return greet; } });\n` +
    `function greet(n) { return "hi " + n; }\n`,
);
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "optdep-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/main.js"></script></body></html>`,
);
fs.writeFileSync(path.join(app, "main.js"), `import { greet } from "cjs-lib";\nwindow.__RESULT = greet("world");\n`);

const get = async (route) => {
  const res = await fetch(`http://localhost:${port}${route}`);
  return { status: res.status, body: await res.text() };
};

let server;
let failed = false;
try {
  server = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
  await waitUp(`http://localhost:${port}/`);

  const main = await get("/main.js");
  assert.equal(main.status, 200);
  // Vite's ensureVersionQuery: the dep URL carries `?v=<browserHash>` so the
  // browser may cache it forever (a re-optimize changes the hash, so the URL).
  const depUrl = main.body.match(/import \* as __ojns0 from "(\/@oj-deps\/cjs-lib\.mjs\?v=[0-9a-f]{8})"/)?.[1];
  assert.ok(depUrl, `consumer redirected to the versioned optimized dep:\n${main.body}`);
  assert.match(main.body, /const \{ greet[^}]*\} = __ojcjs0/, "named import destructured from the cjs value");

  const depRes = await fetch(`http://localhost:${port}${depUrl}`);
  const dep = { status: depRes.status, body: await depRes.text() };
  assert.equal(dep.status, 200);
  assert.match(dep.body, /export default require_cjs_lib\(\)/, "optimized dep exposes module.exports as default");
  // transform middleware: isDep -> `max-age=31536000,immutable`, plus an ETag
  // that answers a conditional request with 304.
  assert.equal(depRes.headers.get("cache-control"), "max-age=31536000,immutable", "versioned dep is immutable");
  const etag = depRes.headers.get("etag");
  assert.ok(etag && etag.startsWith('"'), `dep response carries an ETag, got ${etag}`);
  const revalidated = await fetch(`http://localhost:${port}${depUrl}`, { headers: { "if-none-match": etag } });
  assert.equal(revalidated.status, 304, "matching If-None-Match -> 304");
  // The unversioned URL (a chunk imported relatively from inside the dep dir)
  // stays revalidated on every load, as in Vite.
  const bare = await fetch(`http://localhost:${port}/@oj-deps/cjs-lib.mjs`);
  assert.equal(bare.headers.get("cache-control"), "no-cache", "unversioned dep URL is no-cache");

  const mod = await import(pathToFileURL(path.join(app, ".oj-cache", "v1", "deps", "cjs-lib.mjs")).href);
  const { greet } = mod.default;
  assert.equal(greet("world"), "hi world", "defineProperty export resolves at runtime through interop");

  const manifest = JSON.parse(fs.readFileSync(path.join(app, ".oj-cache", "v1", "deps", "manifest.json"), "utf8"));
  assert.equal(manifest.metadata["cjs-lib"].needsInterop, true);

  console.log("dep-optimize e2e PASSED");
} catch (err) {
  failed = true;
  console.error("dep-optimize e2e FAILED:", err.message);
} finally {
  if (server) server.kill("SIGKILL");
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
