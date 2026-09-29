// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Verifies the TanStack start dev server emits the editor narration
// frames on /__ws (the channel the editor reads): lovable:dev-server-mode and
// lovable:boot-progress on connect, and lovable:update-progress (open done:false
// -> close done:true, trigger "watch") around the rebuild triggered by a source
// edit. Without these the editor's boot pill and "Applying changes…" pill stay
// dark under oj.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const app = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const installed =
  fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(app, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP start narration: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });

const port = 3099;
const routeFile = (() => {
  const dir = path.join(app, "src", "routes");
  const hit = fs.readdirSync(dir).find((f) => f.endsWith(".tsx"));
  if (!hit) throw new Error("no .tsx route file in the fixture");
  return path.join(dir, hit);
})();

async function up() {
  return waitUp(`http://localhost:${port}/`).then(
    () => true,
    () => false,
  );
}
// Deadline-based wait, retouching the probe write on the way: the watcher
// thread may register its watches after HTTP-ready on a loaded runner, so a
// single write can predate registration and never be seen; rewriting the
// same bytes bumps the mtime and re-fires it.
async function waitFor(frames, pred, { ms = 45000, touch } = {}) {
  const deadline = Date.now() + ms;
  let touched = Date.now();
  while (!frames.some(pred)) {
    if (Date.now() >= deadline) return false;
    if (touch && Date.now() - touched >= 3000) {
      touch();
      touched = Date.now();
    }
    await sleep(100);
  }
  return true;
}
const isEvent = (f, ev) => f?.type === "custom" && f?.event === ev;

let failed = false;
let srv, ws;
try {
  srv = spawn(oj, ["dev", app, "--port", String(port)], {
    stdio: "ignore",
    env: { ...process.env, OJ_HMR_GATE: "1" },
  });
  if (!(await up())) throw new Error("start dev server did not come up");

  const frames = [];
  ws = new WebSocket(`ws://localhost:${port}/__ws`);
  ws.addEventListener("message", (e) => {
    try {
      frames.push(JSON.parse(e.data));
    } catch {}
  });
  await new Promise((res, rej) => {
    ws.addEventListener("open", () => res());
    ws.addEventListener("error", () => rej(new Error("ws connect failed")));
  });
  await sleep(400);

  // Connect-time frames: dev-server-mode + boot-progress.
  if (!frames.some((f) => isEvent(f, "lovable:dev-server-mode"))) {
    throw new Error(`no dev-server-mode on connect; got ${JSON.stringify(frames)}`);
  }
  const boot = frames.find((f) => isEvent(f, "lovable:boot-progress"));
  if (!boot) throw new Error(`no boot-progress on connect; got ${JSON.stringify(frames)}`);
  if (typeof boot.data.ssrModules !== "number" || typeof boot.data.clientModules !== "number") {
    throw new Error(`boot-progress missing required module counts: ${JSON.stringify(boot.data)}`);
  }
  console.log("connect frames:   dev-server-mode + boot-progress ok");

  // Edit a route source file -> the watcher rebuilds and narrates an update
  // batch. Identical bytes per retouch: one logical change, re-fired on mtime.
  frames.length = 0;
  const probed = fs.readFileSync(routeFile, "utf8") + `\n// narration probe ${Date.now()}\n`;
  const touch = () => fs.writeFileSync(routeFile, probed);
  touch();

  const opened = await waitFor(frames, (f) => isEvent(f, "lovable:update-progress") && f.data.done === false, {
    touch,
  });
  if (!opened) throw new Error(`no update-progress open frame; got ${JSON.stringify(frames)}`);
  const done = await waitFor(frames, (f) => isEvent(f, "lovable:update-progress") && f.data.done === true);
  if (!done) throw new Error(`no update-progress done frame; got ${JSON.stringify(frames)}`);

  const doneFrame = frames.filter((f) => isEvent(f, "lovable:update-progress") && f.data.done).pop().data;
  if (doneFrame.trigger !== "watch") throw new Error(`update trigger wrong: ${doneFrame.trigger}`);
  if (typeof doneFrame.batch !== "number" || doneFrame.batch < 1) throw new Error("update batch invalid");
  if (typeof doneFrame.ssrModules !== "number" || typeof doneFrame.clientModules !== "number") {
    throw new Error(`update-progress missing required module counts: ${JSON.stringify(doneFrame)}`);
  }
  if (doneFrame.clientModules < 1) throw new Error("update-progress clientModules should be > 0 after a rebuild");
  console.log(
    `update-progress:  open -> done (batch ${doneFrame.batch}, ${doneFrame.clientModules} client modules) ok`,
  );
  console.log("\nSTART NARRATION VERIFIED: editor boot + update frames emitted on /__ws");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  try {
    ws?.close();
  } catch {}
  if (srv) srv.kill("SIGKILL");
  // Undo the probe edit so the fixture stays pristine.
  try {
    const src = fs.readFileSync(routeFile, "utf8").replace(/\n\/\/ narration probe \d+\n/g, "");
    fs.writeFileSync(routeFile, src);
  } catch {}
}
process.exit(failed ? 1 : 0);
