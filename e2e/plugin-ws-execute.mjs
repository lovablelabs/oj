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

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-exec-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "exec-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `let seq = 0;
   const pending = new Map();
   export default [{
     name: "exec-bridge",
     configureServer(server) {
       server.ws.on("exec:result", (data) => {
         const p = pending.get(data.requestId);
         if (p) { pending.delete(data.requestId); p(data.value); }
       });
       server.middlewares.use((req, res, next) => {
         if (req.method !== "POST" || req.url !== "/exec") return next();
         const requestId = String(++seq);
         const done = new Promise((resolve) => {
           pending.set(requestId, resolve);
           setTimeout(() => { pending.delete(requestId); resolve(null); }, 4000);
         });
         server.ws.send("exec:run", { requestId, code: "40 + 2" });
         done.then((value) => {
           res.setHeader("content-type", "application/json");
           res.statusCode = 200;
           res.end(JSON.stringify({ requestId, value }));
         });
       });
     },
   }];\n`,
);
fs.writeFileSync(path.join(app, "src.js"), `window.__READY = true;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src.js"></script></body></html>`,
);

const port = 5497;
let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${port}/`);

  const ws = new WebSocket(`ws://localhost:${port}/__ws`);
  await new Promise((res, rej) => {
    ws.addEventListener("open", res);
    ws.addEventListener("error", rej);
  });
  ws.addEventListener("message", (e) => {
    let m;
    try {
      m = JSON.parse(e.data);
    } catch {
      return;
    }
    if (m.type === "custom" && m.event === "exec:run") {
      const value = eval(m.data.code);
      ws.send(JSON.stringify({ type: "custom", event: "exec:result", data: { requestId: m.data.requestId, value } }));
    }
  });
  await sleep(300);

  const out = await (await fetch(`http://localhost:${port}/exec`, { method: "POST" })).json();
  assert.equal(out.value, 42, "plugin broadcast -> client eval -> plugin collect -> http response round-trips");
  ws.close();
  console.log("PLUGIN-WS-EXECUTE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PLUGIN-WS-EXECUTE E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
