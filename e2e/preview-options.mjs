// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// `oj preview` honors Vite's preview options (each inherited from `server`
// unless set under `preview`): cors, allowedHosts (403 on a foreign Host),
// headers, strictPort, appType mpa (no index.html fallback), and the build's
// assetsDir for immutable caching.

import { execSync, spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { settles } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const PORT = 6511;
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-previewopts-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "previewopts", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "main.js"), `window.__P = 1;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head></head><body><div id="root"></div><script type="module" src="/src/main.js"></script></body></html>`,
);
fs.writeFileSync(path.join(app, "about.html"), `<!doctype html><html><body>about</body></html>`);

function request(p, headers = {}) {
  return new Promise((resolve, reject) => {
    const req = http.request({ host: "127.0.0.1", port: PORT, path: p, method: "GET", headers }, (res) => {
      let body = "";
      res.on("data", (c) => (body += c));
      res.on("end", () => resolve({ status: res.statusCode, headers: res.headers, body }));
    });
    req.on("error", reject);
    req.end();
  });
}
async function startPreview(args = []) {
  const srv = spawn(oj, ["preview", app, "--port", String(PORT), ...args], { stdio: ["ignore", "pipe", "pipe"] });
  let err = "";
  srv.stderr.on("data", (d) => (err += d));
  if (
    await settles(async () => {
      try {
        return Boolean((await request("/")).status);
      } catch {
        return false;
      }
    })
  ) {
    return srv;
  }
  throw new Error(`preview did not start: ${err}`);
}
function build(config) {
  fs.writeFileSync(path.join(app, "oj.config.json"), JSON.stringify(config));
  fs.rmSync(path.join(app, "dist"), { recursive: true, force: true });
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  // A page that is not a build input, served as a plain static file.
  fs.copyFileSync(path.join(app, "about.html"), path.join(app, "dist", "about.html"));
}

let failed = false;
let srv = null;
try {
  // 1. spa (default) + cors/allowedHosts/headers from `server`, headers from `preview` winning.
  build({
    server: { cors: true, allowedHosts: ["ok.test"], headers: { "x-s": "s" } },
    preview: { headers: { "x-p": "p" } },
  });
  srv = await startPreview();
  let r = await request("/", { origin: "http://elsewhere.test" });
  assert.equal(r.status, 200);
  assert.equal(r.headers["access-control-allow-origin"], "http://elsewhere.test", "cors: true reflects the origin");
  assert.equal(r.headers["x-p"], "p", "preview.headers applied");
  assert.equal(
    r.headers["x-s"],
    undefined,
    "preview.headers replaces server.headers (Vite: preview.headers ?? server.headers)",
  );
  r = await request("/", { host: "evil.test" });
  assert.equal(r.status, 403, "a Host outside allowedHosts is refused (DNS rebinding guard)");
  r = await request("/", { host: "ok.test" });
  assert.equal(r.status, 200, "an allowed Host is served");
  r = await request("/deep/route");
  assert.equal(r.status, 200, "spa falls back to index.html");
  assert.match(r.body, /id="root"/);
  r = await request("/about");
  assert.match(r.body, /about/, "/about serves about.html");
  // strictPort: a second preview on the same port exits instead of moving on.
  const clash = spawnSync(oj, ["preview", app, "--port", String(PORT), "--strictPort"], {
    encoding: "utf8",
    timeout: 20000,
  });
  assert.notEqual(clash.status, 0, "--strictPort exits when the port is taken");
  assert.match(clash.stderr, /already in use/);
  srv.kill();
  srv = null;
  console.log("ok: cors, allowedHosts, headers, spa fallback, --strictPort");

  // 2. appType mpa: no index.html fallback; cors: false sends no CORS headers;
  // allowedHosts: true accepts any Host; assetsDir is cached as immutable.
  build({ appType: "mpa", build: { assetsDir: "static" }, server: { cors: false, allowedHosts: true } });
  srv = await startPreview();
  r = await request("/deep/route", { host: "anything.test", origin: "http://elsewhere.test" });
  assert.equal(r.status, 404, "mpa: unknown paths are 404, not index.html");
  assert.equal(r.headers["access-control-allow-origin"], undefined, "cors: false adds no CORS headers");
  r = await request("/", { host: "anything.test" });
  assert.equal(r.status, 200, "allowedHosts: true accepts any Host");
  r = await request("/about");
  assert.equal(r.status, 200, "mpa still maps /about to about.html");
  const asset = fs.readdirSync(path.join(app, "dist", "static")).find((f) => f.endsWith(".js"));
  r = await request(`/static/${asset}`);
  assert.equal(r.status, 200);
  assert.match(r.headers["cache-control"] ?? "", /immutable/, "hashed files under build.assetsDir are immutable");
  srv.kill();
  srv = null;
  console.log("ok: appType mpa, cors: false, allowedHosts: true, assetsDir caching");
} catch (e) {
  failed = true;
  console.error("PREVIEW-OPTIONS E2E FAILED:", e.message);
} finally {
  if (srv) srv.kill();
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
