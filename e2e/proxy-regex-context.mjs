// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// `server.proxy` contexts that start with `^` are regular expressions in Vite,
// tested against the request url (path plus query), not path prefixes. A prefix
// context still wins by length; a regex context applies when no prefix matches.

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
const oj = path.join(repo, "target", "debug", "oj");
const PORT = 6113;
const BACKEND = 6114;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-proxy-re-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "proxy-re-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "vite.config.mjs"),
  `export default { server: { proxy: {\n` +
    `  "/api": "http://localhost:${BACKEND}",\n` +
    `  "^/re/.*": { target: "http://localhost:${BACKEND}" },\n` +
    `  "^/search\\\\?q=": "http://localhost:${BACKEND}",\n` +
    `} } };\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body>INDEX</body></html>`,
);

const backend = http.createServer((req, res) => res.end("BACKEND:" + req.url));

let failed = false;
let srv;
try {
  await new Promise((r) => backend.listen(BACKEND, r));
  srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
  await waitUp(`http://localhost:${PORT}/`);

  const get = async (p) => (await fetch(`http://localhost:${PORT}${p}`)).text();
  assert.equal(await get("/api/hello"), "BACKEND:/api/hello", "prefix context");
  assert.equal(await get("/re/anything?x=1"), "BACKEND:/re/anything?x=1", "regex context");
  assert.equal(await get("/search?q=oj"), "BACKEND:/search?q=oj", "regex context over the query");
  const notRe = await get("/search");
  assert.doesNotMatch(notRe, /^BACKEND:/, `/search without the query must not be proxied: ${notRe}`);
  const outside = await get("/other/re/x");
  assert.doesNotMatch(outside, /^BACKEND:/, `anchored regex must not match a substring: ${outside}`);
  console.log("PROXY-REGEX-CONTEXT E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PROXY-REGEX-CONTEXT E2E FAILED:", err.message);
} finally {
  if (srv) srv.kill("SIGKILL");
  backend.close();
  // The SIGKILLed server's node children flush for a beat and race the removal
  // (ENOTEMPTY); retry like proxy-target-forms.mjs / start-cloudflare-dev.mjs.
  for (let i = 0; ; i++) {
    try {
      fs.rmSync(app, { recursive: true, force: true });
      break;
    } catch {
      if (i >= 20) break;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100);
    }
  }
}
process.exit(failed ? 1 : 0);
