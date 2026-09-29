// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Edge cases of the single (plugin-host) proxy, driven through a vite-format
// app so the app's REAL config reaches the Node proxy (a URL/object target and
// a function `bypass` cannot cross the JSON config bridge — they live only in
// the loaded vite config). The app has no plugins; the host is kept alive
// solely to run `server.proxy`.
//
//   1. A `{ target: new URL(...) }` entry proxies (non-string target accepted).
//   2. A `{ target: { protocol, host, port } }` object entry proxies.
//   3. A `bypass` returning a STRING serves that path via normal routing (200),
//      not the host stack's fallthrough 404.
//   4. A `bypass` returning undefined proxies (with the rewrite applied).

import { spawn, execSync } from "node:child_process";
import http from "node:http";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = 6870; // oj dev server
const BACKEND = 6871; // upstream

if (!fs.existsSync(path.join(fixture, "node_modules", "vite"))) {
  console.log("SKIP proxy target forms: fixture deps not installed (needs vite)");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-proxy-forms-"));
const keep = !!process.env.OJ_E2E_KEEP;
// The SIGKILLed server's node children flush for a beat and race the removal
// (ENOTEMPTY); retry like start-cloudflare-dev.mjs does.
const cleanup = () => {
  if (keep) return;
  for (let i = 0; ; i++) {
    try {
      return fs.rmSync(tmp, { recursive: true, force: true });
    } catch (e) {
      if (i >= 20) return;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100);
    }
  }
};

fs.writeFileSync(
  path.join(tmp, "package.json"),
  JSON.stringify({ name: "proxy-forms", private: true, type: "module" }),
);
fs.symlinkSync(path.join(fixture, "node_modules"), path.join(tmp, "node_modules"), "dir");
fs.writeFileSync(
  path.join(tmp, "index.html"),
  "<!doctype html><html><head><title>t</title></head><body>home</body></html>",
);
fs.writeFileSync(path.join(tmp, "served.html"), "<!doctype html><html><body>SERVED-LOCALLY</body></html>");
fs.writeFileSync(
  path.join(tmp, "vite.config.ts"),
  [
    'import { defineConfig } from "vite";',
    "",
    "export default defineConfig({",
    "  server: {",
    "    proxy: {",
    "      // A URL instance target (non-string).",
    `      "/urlt": { target: new URL("http://127.0.0.1:${BACKEND}"), changeOrigin: true, rewrite: (p) => p.replace(/^\\/urlt/, "") },`,
    "      // An object target {protocol, host, port}.",
    `      "/objt": { target: { protocol: "http", host: "127.0.0.1", port: ${BACKEND} }, changeOrigin: true, rewrite: (p) => p.replace(/^\\/objt/, "") },`,
    "      // A function bypass: a string result is served locally, undefined proxies.",
    `      "/byp": { target: "http://127.0.0.1:${BACKEND}", changeOrigin: true, bypass: (req) => (req.url === "/byp/local" ? "/served.html" : undefined), rewrite: (p) => p.replace(/^\\/byp/, "") },`,
    "    },",
    "  },",
    "});",
    "",
  ].join("\n"),
);

const seen = [];
const backend = http.createServer((req, res) => {
  seen.push(req.url);
  res.setHeader("content-type", "application/json");
  // A real upstream that (adversarially or coincidentally) emits oj's internal
  // signal headers must NOT be misread as the host stack's fallthrough: the
  // proxy strips them, so these responses still proxy through cleanly.
  res.setHeader("x-oj-fallthrough", "1");
  res.setHeader("x-oj-rewritten-url", "/evil");
  res.end(JSON.stringify({ ok: true, path: req.url }));
});

const get = async (route) => {
  const res = await fetch(`http://127.0.0.1:${PORT}${route}`);
  return { status: res.status, body: await res.text() };
};

let failed = false;
let srv;
try {
  await new Promise((r) => backend.listen(BACKEND, "127.0.0.1", r));
  const logFd = fs.openSync(path.join(tmp, "server.log"), "w");
  srv = spawn(oj, ["dev", tmp, "--port", String(PORT)], { stdio: ["ignore", logFd, logFd] });
  await waitUp(`http://127.0.0.1:${PORT}/`);

  // 1. URL-instance target.
  const urlt = await get("/urlt/data");
  assert.equal(urlt.status, 200, `/urlt/data status ${urlt.status}`);
  assert.match(urlt.body, /"ok":true/, `URL-target entry did not proxy: ${urlt.body}`);
  assert.match(urlt.body, /"path":"\/data"/, `URL-target rewrite not applied: ${urlt.body}`);

  // 2. Object target.
  const objt = await get("/objt/data");
  assert.equal(objt.status, 200, `/objt/data status ${objt.status}`);
  assert.match(objt.body, /"ok":true/, `object-target entry did not proxy: ${objt.body}`);
  assert.match(objt.body, /"path":"\/data"/, `object-target rewrite not applied: ${objt.body}`);

  // 3. bypass returning a string -> served locally (200), NOT a 404.
  const byp = await get("/byp/local");
  assert.equal(byp.status, 200, `bypass-string should serve locally, got ${byp.status}`);
  assert.match(byp.body, /SERVED-LOCALLY/, `bypass-string did not serve the rewritten path: ${byp.body.slice(0, 200)}`);

  // 4. bypass returning undefined -> proxied with the rewrite applied.
  const other = await get("/byp/other");
  assert.equal(other.status, 200, `/byp/other status ${other.status}`);
  assert.match(other.body, /"path":"\/other"/, `bypass-undefined did not proxy+rewrite: ${other.body}`);

  assert.ok(seen.includes("/data"), `upstream never saw the stripped /data: ${JSON.stringify(seen)}`);
  console.log("PROXY-TARGET-FORMS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PROXY-TARGET-FORMS E2E FAILED:", err.message);
} finally {
  if (srv) srv.kill("SIGKILL");
  backend.close();
  cleanup();
}
process.exit(failed ? 1 : 0);
