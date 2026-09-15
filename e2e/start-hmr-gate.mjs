// SPDX-License-Identifier: MIT
// The editor-driven HMR gate (`server.hmrGate: true`) must hold a TanStack Start
// page reload the way it holds plain-mode updates: a source write still rebuilds
// the client bundle, but the iframe's reload waits for POST /__hmr_flush (an
// editor's gate plugin for Vite holds a bundled dev server's full-reload the same
// way). Without the gate the reload is immediate.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "..");
const app = path.join(repo, "e2e", "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 5238);
const installed = fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start"));
if (!installed) {
  console.log("SKIP start-hmr-gate: fixture deps not installed");
  process.exit(0);
}
execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const must = (cond, msg) => { if (!cond) throw new Error(msg); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const waitUp = async () => {
  for (let i = 0; i < 240; i++) {
    try { if ((await fetch(`http://localhost:${PORT}/`)).ok) return; } catch {}
    await sleep(500);
  }
  throw new Error(`server on :${PORT} did not start`);
};
const aboutFile = path.join(app, "src", "routes", "about.tsx");
const original = fs.readFileSync(aboutFile, "utf8");
// The gate is a server option; a temporary oj.config.json next to the fixture's
// vite.config turns it on for the gated run only.
const gateConfig = path.join(app, "oj.config.json");

// Connect to the Start live-reload socket and collect "reload" frames.
async function reloadListener() {
  const ws = new WebSocket(`ws://localhost:${PORT}/@oj-start/hmr`);
  const reloads = [];
  ws.addEventListener("message", (ev) => { if (String(ev.data) === "reload") reloads.push(Date.now()); });
  await new Promise((resolve, reject) => { ws.addEventListener("open", resolve); ws.addEventListener("error", reject); });
  return { reloads, close: async () => {
    const closed = new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("Start HMR socket did not close without a reload")), 2000);
      ws.addEventListener("close", (event) => {
        clearTimeout(timer);
        if (event.wasClean) resolve();
        else reject(new Error("Start HMR socket closed without completing the close handshake"));
      }, { once: true });
    });
    ws.close();
    await closed;
  } };
}

async function run(label, gated, check) {
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  fs.rmSync(gateConfig, { force: true });
  if (gated) fs.writeFileSync(gateConfig, JSON.stringify({ server: { hmrGate: true } }));
  const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
  let log = "";
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  try {
    await waitUp();
    await (await reloadListener()).close();
    const listener = await reloadListener();
    // A real source change: the watcher rebuilds the client bundle.
    fs.writeFileSync(aboutFile, original + `\n// gate probe ${label} ${Date.now()}\n`);
    await check(listener, () => log);
    await listener.close();
  } finally {
    fs.writeFileSync(aboutFile, original);
    fs.rmSync(gateConfig, { force: true });
    srv.kill("SIGKILL");
    await sleep(300);
  }
}

// 1. Gate on: the rebuild happens, the reload is held, the status shows it, the
//    flush releases exactly one reload.
await run("gated", true, async (listener, log) => {
  const deadline = Date.now() + 30000;
  while (Date.now() < deadline && !log().includes("reload held")) await sleep(200);
  must(log().includes("oj start: rebuilt, reload held"), `gated: the watcher should rebuild and hold the reload:\n${log().slice(-1200)}`);
  await sleep(1500);
  must(listener.reloads.length === 0, `gated: a reload reached the page before the flush (${listener.reloads.length})`);
  const status = await (await fetch(`http://localhost:${PORT}/__hmr_gate`)).json();
  must(status.enabled === true && status.heldReload === true && status.count >= 1, `gated: /__hmr_gate should report the held reload, got ${JSON.stringify(status)}`);
  must(typeof status.startedAt === "number" && status.startedAt > 0, "gated: /__hmr_gate carries startedAt for the editor's restart detection");
  const flushed = await (await fetch(`http://localhost:${PORT}/__hmr_flush`, { method: "POST" })).json();
  must(flushed.reload === true, `gated: the flush response should say a reload was released, got ${JSON.stringify(flushed)}`);
  const until = Date.now() + 10000;
  while (Date.now() < until && listener.reloads.length === 0) await sleep(100);
  must(listener.reloads.length === 1, `gated: expected exactly one reload after the flush, got ${listener.reloads.length}`);
  const after = await (await fetch(`http://localhost:${PORT}/__hmr_gate`)).json();
  must(after.heldReload === false && after.count === 0, `gated: the gate should be empty after the flush, got ${JSON.stringify(after)}`);
  console.log("gated: rebuild held, status reported it, flush released one reload");
});

// 2. Gate off: the reload follows the rebuild at once.
await run("plain", false, async (listener) => {
  // An environment that enables the gate globally makes this half moot.
  if ((await (await fetch(`http://localhost:${PORT}/__hmr_gate`)).json()).enabled) {
    console.log("plain: the gate is enabled by the environment, skipping the immediate-reload check");
    return;
  }
  const until = Date.now() + 30000;
  while (Date.now() < until && listener.reloads.length === 0) await sleep(100);
  must(listener.reloads.length >= 1, "plain: the rebuild should reload the page without a gate");
  console.log("plain: rebuild reloads immediately");
});

console.log("START HMR GATE E2E PASSED");
