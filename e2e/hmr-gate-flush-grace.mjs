// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// The editor HMR gate and a flush that races its write. An editor writes,
// then POSTs /__hmr_flush; when the flush lands before the write (or before
// its watcher event), it releases nothing, and the write used to be held for
// the hold cap (minutes) waiting for a flush already sent. A flush that
// released nothing now lets writes pass for a short window. A flush that
// released something opens none: the editor's next write is held as always.
//
// The gate comes from the app's config: an editor's `hmr-gate` plugin turns
// oj's native gate on, and the plugin itself is never hosted, so one change
// is never gated twice.

import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { sleep, waitUp } from "./util.mjs";

const OJ = process.env.OJ_BIN ?? path.join(process.cwd(), "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 5338);
const base = `http://localhost:${PORT}`;
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-flushgrace-"));
const color = path.join(app, "src", "color.js");

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), '{"name":"flushgrace","private":true,"type":"module"}');
fs.writeFileSync(
  path.join(app, "index.html"),
  '<!doctype html><html><body><div id="app"></div><script type="module" src="/src/main.js"></script></body></html>',
);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  'import { color } from "./color.js";\ndocument.getElementById("app").textContent = color;\nif (import.meta.hot) import.meta.hot.accept();\n',
);
fs.writeFileSync(color, 'export const color = "c0";\n');
// `keep-host` keeps the plugin host up; `hmr-gate` must not be hosted.
fs.writeFileSync(
  path.join(app, "vite.config.mjs"),
  `export default { plugins: [
  { name: "keep-host", configureServer() {} },
  { name: "hmr-gate", configureServer() { throw new Error("hmr-gate plugin hosted"); } },
] };\n`,
);

let log = "";
const srv = spawn(OJ, ["dev", "--port", String(PORT)], {
  cwd: app,
  stdio: ["ignore", "pipe", "pipe"],
  // No OJ_HMR_GATE: the plugin alone turns the gate on.
  env: { ...process.env, OJ_HMR_GATE: "", OJ_HMR_FULL_RELOAD: "false" },
});
srv.stdout.on("data", (d) => (log += d));
srv.stderr.on("data", (d) => (log += d));

let failed = false;
let ws;
try {
  await waitUp(`${base}/`, { proc: srv });
  const frames = [];
  ws = new WebSocket(`ws://localhost:${PORT}/__ws`, "vite-hmr");
  ws.addEventListener("message", (e) => frames.push(JSON.parse(e.data)));
  await new Promise((resolve, reject) => {
    ws.addEventListener("open", resolve);
    ws.addEventListener("error", reject);
  });
  // Load the graph so color.js has a boundary.
  for (const p of ["/", "/src/main.js", "/src/color.js"]) await (await fetch(base + p)).text();
  await sleep(500);
  must(/plugin host: .*keep-host/.test(log), `the plugin host did not come up:\n${log}`);
  must(!/plugin host: .*\bhmr-gate\b/.test(log), `the hmr-gate plugin was hosted:\n${log}`);
  must(!log.includes("hmr-gate plugin hosted"), `the hmr-gate plugin ran:\n${log}`);
  must(log.includes("hmr gate: on (granular)"), `the hmr-gate plugin did not turn oj's gate on:\n${log}`);

  const flush = async () => (await fetch(`${base}/__hmr_flush`, { method: "POST" })).json();
  const pending = async () => (await (await fetch(`${base}/__hmr_gate`)).json()).count;
  let n = 0;
  const write = () => fs.writeFileSync(color, `export const color = "c${++n}";\n`);
  const updated = async (ms) => {
    const until = Date.now() + ms;
    while (Date.now() < until) {
      if (frames.some((f) => f.type === "update" || f.type === "full-reload")) return true;
      await sleep(50);
    }
    return false;
  };

  for (let round = 1; round <= 3; round++) {
    // 1. Write, then flush: held, then released.
    frames.length = 0;
    write();
    await sleep(300);
    must((await pending()) === 1, `round ${round}: the write was not held`);
    must(!(await updated(0)), `round ${round}: an update passed the gate before the flush`);
    must((await flush()).count === 1, `round ${round}: the flush released nothing`);
    must(await updated(3000), `round ${round}: no update after the flush`);

    // 2. The flush lands first, releasing nothing; the write follows at once.
    await sleep(800); // past any window the previous flush opened
    frames.length = 0;
    const racing = flush();
    await sleep(5);
    write();
    must((await racing).count === 0, `round ${round}: the racing flush released something`);
    must(await updated(3000), `round ${round}: a write right after an empty flush was stranded`);
    must((await pending()) === 0, `round ${round}: the gate kept the racing write`);

    // 3. A flush that released something opens no window.
    await sleep(800);
    frames.length = 0;
    write();
    await sleep(300);
    must((await flush()).count === 1, `round ${round}: the setup flush released nothing`);
    await updated(3000);
    frames.length = 0;
    await sleep(50);
    write();
    await sleep(300);
    must((await pending()) === 1, `round ${round}: the next write passed the gate after a releasing flush`);
    await flush();
    must(await updated(3000), `round ${round}: the next write was not released`);
    await sleep(800);
  }
  console.log("ok: the hmr-gate plugin turns oj's gate on unhosted; a write racing an empty flush passes; the next write is still held");
} catch (e) {
  failed = true;
  console.error(String(e?.stack ?? e));
  console.error(`--- oj log (tail)\n${log.slice(-3000)}`);
} finally {
  ws?.close();
  srv.kill("SIGKILL");
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
