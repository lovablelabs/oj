// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// OJ_DEBUG_MEM=1 exposes /@oj/debug/gc, which forces a full V8 collection in
// every live engine (the memory-probing instrument from issue #202: measure
// retained heap, not whatever V8 has not collected yet). Without the env the
// route must 404 — it is an instrument, never part of the dev surface.
// Run with a built target/debug/oj.
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.resolve(process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj"));

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-debug-gc-"));
const write = (rel, contents) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), contents);
};
write("package.json", JSON.stringify({ name: "debug-gc-app", version: "1.0.0" }));
// A plugin, so the client plugin host spawns and the gc reaches a real engine.
write("oj.plugins.mjs", `export default [{ name: "noop", transform() { return null; } }];\n`);
write("src/main.js", `document.body.textContent = "ok";\n`);
write("index.html", `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`);

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function boot(port, env) {
  const proc = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore", env: { ...process.env, ...env } });
  for (let i = 0; i < 200; i++) {
    try {
      if ((await fetch(`http://localhost:${port}/`)).ok) return proc;
    } catch {}
    await sleep(50);
  }
  throw new Error("no server");
}

let failed = false;
let proc;
try {
  // Enabled: 200 with a requested count, >=1 once the plugin host is up.
  proc = await boot(5464, { OJ_DEBUG_MEM: "1" });
  await fetch(`http://localhost:5464/src/main.js`); // nudge the plugin host awake
  let requested = 0;
  for (let i = 0; i < 40 && requested === 0; i++) {
    const res = await fetch(`http://localhost:5464/@oj/debug/gc`);
    if (res.status !== 200) throw new Error(`enabled: expected 200, got ${res.status}`);
    ({ requested } = await res.json());
    if (requested === 0) await sleep(250);
  }
  if (requested < 1) throw new Error("gc never reached a live engine");
  proc.kill("SIGKILL");
  await new Promise((r) => proc.on("exit", r));

  // Disabled: the route does not exist.
  proc = await boot(5465, {});
  const res = await fetch(`http://localhost:5465/@oj/debug/gc`);
  if (res.status !== 404) throw new Error(`disabled: expected 404, got ${res.status}`);
  console.log("DEBUG GC E2E PASSED");
} catch (err) {
  failed = true;
  console.error("DEBUG GC E2E FAILED:", err.message);
} finally {
  if (proc) {
    proc.kill("SIGKILL");
    await new Promise((r) => proc.on("exit", r));
  }
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
