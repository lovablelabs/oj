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

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-ws-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "ws-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `export default [{
    name: "ws-echo",
    configureServer(server) {
      // Vite: the ws server emits "connection" per accepted client socket;
      // plugins push initial state from it.
      server.ws.on("connection", (socket) => {
        socket.send(JSON.stringify({ type: "custom", event: "server:hello", data: { greeted: true } }));
      });
      server.ws.on("client:ping", (data, client) => {
        server.ws.send("server:pong", { echo: data });
      });
    },
  }];\n`,
);
fs.writeFileSync(path.join(app, "src.js"), `window.__READY = true;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src.js"></script></body></html>`,
);

const port = 5486;
let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${port}/`);

  const ws = new WebSocket(`ws://localhost:${port}/__ws`);
  const hello = new Promise((res, rej) => {
    const timer = setTimeout(() => rej(new Error("timed out waiting for server:hello on connection")), 8000);
    ws.addEventListener("message", (e) => {
      let m;
      try { m = JSON.parse(e.data); } catch { return; }
      if (m.type === "custom" && m.event === "server:hello") {
        clearTimeout(timer);
        res(m.data);
      }
    });
  });
  await new Promise((res, rej) => {
    ws.addEventListener("open", res);
    ws.addEventListener("error", rej);
  });
  assert.deepEqual(await hello, { greeted: true }, "server.ws.on('connection') fires for a new client");

  const pong = await new Promise((res, rej) => {
    const timer = setTimeout(() => rej(new Error("timed out waiting for server:pong")), 8000);
    ws.addEventListener("message", (e) => {
      let m;
      try { m = JSON.parse(e.data); } catch { return; }
      if (m.type === "custom" && m.event === "server:pong") {
        clearTimeout(timer);
        res(m.data);
      }
    });
    ws.send(JSON.stringify({ type: "custom", event: "client:ping", data: { hello: "world" } }));
  });

  assert.deepEqual(pong, { echo: { hello: "world" } }, "plugin ws.on -> ws.send round-trips to the client");
  ws.close();

  // the same relay must work for a client on the vite-hmr socket (origin root)
  const vws = new WebSocket(`ws://localhost:${port}/`, ["vite-hmr"]);
  await new Promise((res, rej) => {
    vws.addEventListener("open", res);
    vws.addEventListener("error", () => rej(new Error("vite-hmr socket open failed")));
  });
  const pong2 = await new Promise((res, rej) => {
    const timer = setTimeout(() => rej(new Error("timed out waiting for server:pong over vite-hmr")), 8000);
    vws.addEventListener("message", (e) => {
      let m;
      try { m = JSON.parse(e.data); } catch { return; }
      if (m.type === "custom" && m.event === "server:pong") {
        clearTimeout(timer);
        res(m.data);
      }
    });
    vws.send(JSON.stringify({ type: "custom", event: "client:ping", data: { hello: "vite" } }));
  });
  assert.deepEqual(pong2, { echo: { hello: "vite" } }, "plugin ws relay round-trips over the vite-hmr socket");
  vws.close();

  console.log("PLUGIN-WS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PLUGIN-WS E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
