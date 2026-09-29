// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite's dev-server request policies: `server.cors` (default: localhost origins
// only, `false` off, an object with exact origins) and `server.allowedHosts`
// (Host header allowlist against DNS rebinding; localhost, IP literals and
// listed hosts pass, everything else is 403). Also the WebSocket Origin check.

import { spawn, execSync } from "node:child_process";
import http from "node:http";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { settles } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const PORT = 5491;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-corshost-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "corshost-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "main.js"), `window.__OK = 1;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

// node:http lets us set Host/Origin freely (fetch forbids Host).
function req(p, headers = {}, method = "GET") {
  return new Promise((resolve, reject) => {
    const r = http.request({ host: "127.0.0.1", port: PORT, path: p, method, headers }, (res) => {
      res.resume();
      res.on("end", () => resolve({ status: res.statusCode, headers: res.headers }));
    });
    r.on("error", reject);
    r.end();
  });
}

async function withServer(config, fn) {
  if (config) fs.writeFileSync(path.join(app, "oj.config.json"), JSON.stringify(config));
  else fs.rmSync(path.join(app, "oj.config.json"), { force: true });
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
  try {
    await settles(async () => { try { return Boolean((await req("/")).status); } catch { return false; } }, { pollMs: 200 });
    await fn();
  } finally {
    srv.kill("SIGKILL");
    await sleep(300);
  }
}

const ACAO = "access-control-allow-origin";
let failed = false;
try {
  await withServer(null, async () => {
    // default cors: localhost origins reflected, others get nothing
    let r = await req("/src/main.js", { Origin: "http://localhost:1234" });
    assert.equal(r.headers[ACAO], "http://localhost:1234", "localhost origin reflected");
    assert.match(String(r.headers.vary), /Origin/);
    r = await req("/src/main.js", { Origin: "http://app.localhost:1234" });
    assert.equal(r.headers[ACAO], "http://app.localhost:1234", "*.localhost origin reflected");
    r = await req("/src/main.js", { Origin: "http://evil.com" });
    assert.equal(r.headers[ACAO], undefined, "foreign origin gets no CORS header");
    r = await req("/src/main.js", { Origin: "http://localhost:1234", "Access-Control-Request-Method": "GET", "Access-Control-Request-Headers": "x-custom" }, "OPTIONS");
    assert.equal(r.status, 204, "preflight is answered");
    assert.match(r.headers["access-control-allow-methods"], /GET/);
    assert.equal(r.headers["access-control-allow-headers"], "x-custom");
    // default host check: localhost / IP literals pass, a foreign Host is blocked
    r = await req("/", { Host: `localhost:${PORT}` });
    assert.equal(r.status, 200);
    r = await req("/", { Host: `127.0.0.1:${PORT}` });
    assert.equal(r.status, 200);
    r = await req("/", { Host: "evil.com" });
    assert.equal(r.status, 403, "foreign Host is blocked (DNS rebinding)");
    r = await req("/__ws", { Host: `localhost:${PORT}`, Origin: "http://evil.com", Connection: "Upgrade", Upgrade: "websocket", "Sec-WebSocket-Version": "13", "Sec-WebSocket-Key": "dGhlIHNhbXBsZSBub25jZQ==" });
    assert.equal(r.status, 403, "ws upgrade from a foreign Origin is refused");
  });

  await withServer({ server: { cors: false, allowedHosts: ["evil.com", ".corp.example"] } }, async () => {
    let r = await req("/src/main.js", { Origin: "http://localhost:1234" });
    assert.equal(r.headers[ACAO], undefined, "cors:false adds no header");
    r = await req("/", { Host: "evil.com" });
    assert.equal(r.status, 200, "allowedHosts entry passes");
    r = await req("/", { Host: "a.b.corp.example:80" });
    assert.equal(r.status, 200, "dotted entry allows subdomains");
    r = await req("/", { Host: "other.example" });
    assert.equal(r.status, 403, "unlisted Host is still blocked");
  });

  await withServer({ server: { cors: { origin: "http://foo.test", credentials: true }, allowedHosts: true } }, async () => {
    let r = await req("/src/main.js", { Origin: "http://foo.test" });
    assert.equal(r.headers[ACAO], "http://foo.test");
    assert.equal(r.headers["access-control-allow-credentials"], "true");
    r = await req("/src/main.js", { Origin: "http://localhost:1234" });
    assert.equal(r.headers[ACAO], undefined, "an explicit origin list replaces the localhost default");
    r = await req("/", { Host: "anything.example" });
    assert.equal(r.status, 200, "allowedHosts:true disables the check");
  });
  console.log("DEV-CORS-HOST E2E PASSED");
} catch (err) {
  failed = true;
  console.error("DEV-CORS-HOST E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
