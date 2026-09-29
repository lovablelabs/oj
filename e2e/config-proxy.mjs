// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

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

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-proxy-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "proxy-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "vite.config.mjs"),
  `export default { server: { proxy: { "/api": "http://localhost:5361" } }, worker: { format: "es" } };\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body>hi</body></html>`,
);

const backend = http.createServer((req, res) => res.end("BACKEND:" + req.url));

let failed = false;
let srv;
try {
  await new Promise((r) => backend.listen(5361, r));
  const log = path.join(app, "server.log");
  const fd = fs.openSync(log, "w");
  srv = spawn(oj, ["dev", app, "--port", "5360"], { stdio: ["ignore", fd, fd] });
  await waitUp("http://localhost:5360/");

  const res = await fetch("http://localhost:5360/api/hello");
  const body = await res.text();
  assert.equal(body, "BACKEND:/api/hello", `proxy did not forward: ${body}`);

  const logText = fs.readFileSync(log, "utf8");
  assert.match(logText, /proxy: \/api/, "oj did not adopt vite.config server.proxy");
  assert.match(logText, /worker config is not applied/, "no warning for ignored config");

  console.log("CONFIG-PROXY E2E PASSED");
} catch (err) {
  failed = true;
  console.error("CONFIG-PROXY E2E FAILED:", err.message);
} finally {
  if (srv) srv.kill("SIGKILL");
  backend.close();
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
