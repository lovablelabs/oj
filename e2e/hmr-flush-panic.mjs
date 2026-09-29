// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Regression: POST /__hmr_flush in GRANULAR gate mode with a plugin host active
// used to panic ("Cannot start a runtime from within a runtime") — decide() ran
// state.rt.block_on(host.watch_files()) on an async worker thread. decide is now
// async and awaited, so the flush must return 200 and the server stay alive.

import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { waitUp } from "./util.mjs";

const OJ = path.join(process.cwd(), "target", "debug", "oj");
const PORT = 5337;
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-flushpanic-"));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

let failed = false;
let srv;
try {
  fs.mkdirSync(path.join(app, "src"), { recursive: true });
  fs.writeFileSync(path.join(app, "package.json"), '{"name":"flushpanic","private":true}');
  fs.writeFileSync(
    path.join(app, "index.html"),
    '<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>',
  );
  fs.writeFileSync(path.join(app, "src", "main.js"), "export const v = 1;\n");
  // A plugin (configureServer-only) keeps the plugin host resident, so decide()
  // calls host.watch_files() during flush — the path that panicked.
  fs.writeFileSync(
    path.join(app, "vite.config.mjs"),
    'export default { plugins: [{ name: "keep-host", configureServer() {} }] };\n',
  );

  srv = spawn(OJ, ["dev", "--port", String(PORT)], {
    cwd: app,
    stdio: "ignore",
    // gate ON + GRANULAR (not full-reload) so flush routes through decide().
    env: { ...process.env, OJ_HMR_GATE: "1", OJ_HMR_FULL_RELOAD: "false" },
  });
  const up = async () => {
    return waitUp(`http://localhost:${PORT}/`).then(
      () => true,
      () => false,
    );
  };
  if (!(await up())) throw new Error("dev server did not start");

  // Edit a watched source file so the gate holds a pending change.
  fs.writeFileSync(path.join(app, "src", "main.js"), "export const v = 2;\n");
  await sleep(1200);

  const status = await (await fetch(`http://localhost:${PORT}/__hmr_gate`)).json();
  if (status.mode !== "granular") throw new Error(`expected granular gate, got ${status.mode}`);
  if (!(status.count >= 1)) throw new Error(`expected a held change, count=${status.count}`);
  console.log(`gate: granular, ${status.count} held`);

  // The flush that used to panic on the async worker thread.
  const res = await fetch(`http://localhost:${PORT}/__hmr_flush`, { method: "POST" });
  if (res.status !== 200) throw new Error(`flush returned ${res.status} (worker likely panicked)`);
  const flush = await res.json();
  if (flush.mode !== "granular") throw new Error(`flush mode: ${flush.mode}`);
  console.log(`flush: 200, mode granular, released ${flush.count}`);

  // Server must still be alive (a worker panic would take the connection down).
  const after = await fetch(`http://localhost:${PORT}/`);
  if (!after.ok) throw new Error(`server not alive after flush: ${after.status}`);
  console.log("server alive after flush: yes");
  console.log("\nHMR FLUSH PANIC REGRESSION VERIFIED");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  if (srv) srv.kill("SIGKILL");
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
