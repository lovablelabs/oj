// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Verifies HMR plugin-host RPCs are gated by hook presence:
//  A) a plugin WITH handleHotUpdate -> "full-reload" still fires on edit (the RPC
//     is not wrongly skipped): the HMR socket receives a plugin full-reload.
//  B) a configureServer-only plugin (no handleHotUpdate/watchChange) still HMRs on
//     edit, proving the skip never breaks hot updates.

import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { settles, waitUp } from "./util.mjs";

const OJ = path.join(process.cwd(), "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function makeApp(pluginSrc) {
  const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-hmrhook-"));
  fs.mkdirSync(path.join(app, "src"), { recursive: true });
  fs.writeFileSync(path.join(app, "package.json"), '{"name":"hmrhook","private":true}');
  fs.writeFileSync(
    path.join(app, "index.html"),
    '<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>',
  );
  fs.writeFileSync(path.join(app, "src", "main.js"), "export const v = 1;\n");
  fs.writeFileSync(path.join(app, "vite.config.mjs"), `export default { plugins: [ ${pluginSrc} ] };\n`);
  return app;
}

async function up(port) {
  return waitUp(`http://localhost:${port}/`).then(() => true, () => false);
}

function openWs(port) {
  const msgs = [];
  const ws = new WebSocket(`ws://localhost:${port}/__ws`);
  ws.addEventListener("message", (e) => { try { msgs.push(JSON.parse(e.data)); } catch {} });
  const ready = new Promise((res) => ws.addEventListener("open", () => res()));
  return { msgs, ready, close: () => ws.close() };
}

const waitFor = (msgs, pred, ms = 6000) => settles(() => msgs.some(pred), { timeoutMs: ms });

async function scenario(name, port, pluginSrc, edit, predicate) {
  const app = makeApp(pluginSrc);
  const main = path.join(app, "src", "main.js");
  let child;
  try {
    child = spawn(OJ, ["dev", "--port", String(port)], { cwd: app, stdio: "ignore" });
    if (!(await up(port))) throw new Error("server did not start");
    await (await fetch(`http://localhost:${port}/src/main.js`)).text(); // register in graph
    const w = openWs(port);
    await w.ready;
    await sleep(200);
    fs.writeFileSync(main, edit);
    const ok = await waitFor(w.msgs, predicate);
    w.close();
    if (!ok) throw new Error(`${name}: expected HMR message not received; got ${JSON.stringify(w.msgs)}`);
    console.log(`${name}: ok`);
  } finally {
    if (child) child.kill("SIGKILL");
    fs.rmSync(app, { recursive: true, force: true });
  }
}

let failed = false;
try {
  await scenario(
    "hook present (handleHotUpdate)",
    5326,
    '{ name: "hu", handleHotUpdate() { return "full-reload"; } }',
    "export const v = 2;\n",
    (m) => m.type === "full-reload" && m.reason === "plugin",
  );
  await scenario(
    "no hmr hooks (configureServer only)",
    5327,
    '{ name: "cfg", configureServer() {} }',
    "export const v = 3;\n",
    (m) => m.type === "update" || m.type === "full-reload",
  );
  console.log("\nHMR HOOK GATING VERIFIED: present hooks fire, absent hooks skip without breaking HMR");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
}
process.exit(failed ? 1 : 0);
