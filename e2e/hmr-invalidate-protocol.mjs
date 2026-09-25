// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Wire shapes a Vite-protocol client relies on:
//  - `import.meta.hot.invalidate()` arrives as the custom event `vite:invalidate`
//    and answers with js-update entries carrying type, acceptedPath and
//    firstInvalidatedBy (never a bare {path, timestamp});
//  - a second invalidate for the same update is ignored, and one that comes back
//    around to the module that started the chain is a full reload
//    ('circular import invalidate');
//  - full-reload frames carry `path` ('/index.html' for an edited page, '*'
//    otherwise) and `triggeredBy`.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const port = Number(process.env.OJ_E2E_PORT || 5488);

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-invalidate-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "invalidate-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "main.js"), `import "./child.js";\nif (import.meta.hot) import.meta.hot.accept();\n`);
const child = (v) => `export const v = ${v};\nif (import.meta.hot) import.meta.hot.accept();\n`;
fs.writeFileSync(path.join(app, "src", "child.js"), child(1));
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

async function waitFor(pred, label, ms = 10000) {
  for (let i = 0; i < ms / 100; i++) {
    if (pred()) return;
    await sleep(100);
  }
  throw new Error(`timeout: ${label}`);
}

const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
let failed = false;
let ws;
try {
  for (let i = 0; i < 80; i++) { try { if ((await fetch(`http://localhost:${port}/`)).ok) break; } catch {} await sleep(200); }

  const frames = [];
  ws = new WebSocket(`ws://localhost:${port}/__ws`);
  ws.addEventListener("message", (ev) => { try { frames.push(JSON.parse(ev.data)); } catch {} });
  await new Promise((resolve, reject) => {
    const to = setTimeout(() => reject(new Error("socket did not open")), 8000);
    ws.addEventListener("open", () => { clearTimeout(to); resolve(); });
    ws.addEventListener("error", () => { clearTimeout(to); reject(new Error("socket errored")); });
  });
  const send = (o) => ws.send(JSON.stringify(o));
  const last = () => frames[frames.length - 1];

  // seed the graph the way a page load would
  await (await fetch(`http://localhost:${port}/src/main.js`)).text();
  await (await fetch(`http://localhost:${port}/src/child.js`)).text();
  await sleep(300);

  // An update touches child (self-accepting): js-update on child itself.
  let n = frames.length;
  fs.writeFileSync(path.join(app, "src", "child.js"), child(2));
  await waitFor(() => frames.length > n && last().type === "update", "child update");
  assert.deepEqual(
    last().updates.map((u) => [u.type, u.path, u.acceptedPath]),
    [["js-update", "/src/child.js", "/src/child.js"]],
  );

  // child invalidates itself (Vite's custom event shape): the update escalates
  // to its importer with the full Vite entry shape.
  n = frames.length;
  send({ type: "custom", event: "vite:invalidate", data: { path: "/src/child.js", message: "m", firstInvalidatedBy: "/src/child.js" } });
  await waitFor(() => frames.length > n, "invalidate answer");
  assert.equal(last().type, "update", `invalidate answers with an update, got ${JSON.stringify(last())}`);
  const entry = last().updates[0];
  assert.equal(entry.type, "js-update");
  assert.equal(entry.path, "/src/main.js");
  assert.equal(entry.acceptedPath, "/src/main.js");
  assert.equal(entry.firstInvalidatedBy, "/src/child.js");
  assert.equal(typeof entry.timestamp, "number");

  // The same invalidate again for the same update is ignored.
  n = frames.length;
  send({ type: "custom", event: "vite:invalidate", data: { path: "/src/child.js", firstInvalidatedBy: "/src/child.js" } });
  await sleep(600);
  assert.equal(frames.length, n, "repeated invalidate for one update produces nothing");

  // A new update re-arms it; an invalidate whose chain started at the boundary
  // the walk reaches again is circular: full reload, not another update.
  n = frames.length;
  fs.writeFileSync(path.join(app, "src", "child.js"), child(3));
  await waitFor(() => frames.length > n && last().type === "update", "child update 2");
  n = frames.length;
  send({ type: "custom", event: "vite:invalidate", data: { path: "/src/child.js", firstInvalidatedBy: "/src/main.js" } });
  await waitFor(() => frames.length > n, "circular invalidate answer");
  assert.equal(last().type, "full-reload", `circular invalidate reloads, got ${JSON.stringify(last())}`);
  assert.equal(last().reason, "circular import invalidate");
  assert.equal(last().path, "*");

  // An edited page names itself; triggeredBy is the absolute file.
  n = frames.length;
  fs.writeFileSync(path.join(app, "index.html"), `<!doctype html><html><head><title>t2</title></head><body><script type="module" src="/src/main.js"></script></body></html>`);
  await waitFor(() => frames.length > n && last().type === "full-reload", "html full-reload");
  assert.equal(last().path, "/index.html");
  assert.equal(fs.realpathSync(last().triggeredBy), fs.realpathSync(path.join(app, "index.html")));

  console.log("HMR-INVALIDATE-PROTOCOL E2E PASSED");
} catch (err) {
  failed = true;
  console.error("HMR-INVALIDATE-PROTOCOL E2E FAILED:", err.message);
} finally {
  try { ws?.close(); } catch {}
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
