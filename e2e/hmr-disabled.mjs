// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

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

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-hmroff-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "hmr-off-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "vite.config.mjs"), `export default { server: { hmr: false } };\n`);
fs.writeFileSync(path.join(app, "src", "main.js"), `document.title = "v1";\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

const port = 5491;
let failed = false;
let log = "";
const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: ["ignore", "pipe", "pipe"] });
srv.stdout.on("data", (d) => (log += d));
srv.stderr.on("data", (d) => (log += d));
try {
  await waitUp(`http://localhost:${port}/`);

  assert.match(log, /hmr: disabled/, "server did not report hmr disabled");

  const frames = [];
  const ws = new WebSocket(`ws://localhost:${port}/__ws`);
  ws.addEventListener("message", (ev) => { try { frames.push(JSON.parse(ev.data)); } catch {} });
  await new Promise((res, rej) => { ws.addEventListener("open", res); ws.addEventListener("error", rej); });

  // seed the module into the graph, then edit it; with hmr off no reload frame
  // should be broadcast.
  await (await fetch(`http://localhost:${port}/src/main.js`)).text();
  await sleep(300);
  fs.writeFileSync(path.join(app, "src", "main.js"), `document.title = "v2";\n`);
  await sleep(2500);

  const reloads = frames.filter((f) => f.type === "full-reload" || f.type === "update");
  assert.equal(reloads.length, 0, `expected no reload frames, got ${JSON.stringify(reloads)}`);
  ws.close();

  console.log("HMR-DISABLED E2E PASSED");
} catch (err) {
  failed = true;
  console.error("HMR-DISABLED E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
