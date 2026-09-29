// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-vitehmr-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "vite-hmr-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "main.js"), `document.title = "v1";\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

async function waitFor(pred, label, ms = 10000) {
  if (!(await settles(pred, { timeoutMs: ms }))) throw new Error(`timeout: ${label}`);
}

const port = 5479;
const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
let failed = false;
try {
  await waitUp(`http://localhost:${port}/`);

  // Vite's client dials the origin root with the "vite-hmr" subprotocol; the
  // socket route lives at /__ws, so a root dial must still upgrade.
  const frames = [];
  const ws = new WebSocket(`ws://localhost:${port}/`, ["vite-hmr"]);
  ws.addEventListener("message", (ev) => {
    try {
      frames.push(JSON.parse(ev.data));
    } catch {}
  });
  await new Promise((resolve, reject) => {
    const to = setTimeout(() => reject(new Error("vite-hmr socket did not open on /")), 8000);
    ws.addEventListener("open", () => {
      clearTimeout(to);
      resolve();
    });
    ws.addEventListener("error", () => {
      clearTimeout(to);
      reject(new Error("vite-hmr socket errored on /"));
    });
  });

  assert.equal(ws.protocol, "vite-hmr", "server echoes the vite-hmr subprotocol");
  await waitFor(() => frames.some((f) => f.type === "connected"), "connected frame");
  assert.equal(frames[0]?.type, "connected", "connected is the first frame");

  // seed the module into the graph, then edit it; the reload frame must reach
  // the root-dialed vite-hmr client.
  await (await fetch(`http://localhost:${port}/src/main.js`)).text();
  await sleep(300);
  const before = frames.length;
  fs.writeFileSync(path.join(app, "src", "main.js"), `document.title = "v2";\n`);
  await waitFor(
    () => frames.slice(before).some((f) => f.type === "full-reload" || f.type === "update"),
    "reload frame after edit",
  );
  ws.close();

  // a real Vite client's reconnect liveness probe uses the vite-ping
  // subprotocol; the server accepts it, then closes it.
  const ping = new WebSocket(`ws://localhost:${port}/`, ["vite-ping"]);
  const pingClosed = await new Promise((resolve, reject) => {
    const to = setTimeout(() => reject(new Error("vite-ping socket did not open")), 8000);
    let opened = false;
    ping.addEventListener("open", () => {
      opened = true;
    });
    ping.addEventListener("close", () => {
      clearTimeout(to);
      resolve(opened);
    });
    ping.addEventListener("error", () => {
      clearTimeout(to);
      reject(new Error("vite-ping socket errored"));
    });
  });
  assert.equal(pingClosed, true, "vite-ping socket opened then was closed by the server");

  console.log("VITE-HMR-SOCKET E2E PASSED");
} catch (err) {
  failed = true;
  console.error("VITE-HMR-SOCKET E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
