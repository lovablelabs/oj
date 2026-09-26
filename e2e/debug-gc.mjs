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
  const childEnv = { ...process.env, ...env };
  // The disabled case must not inherit an OJ_DEBUG_MEM the caller exported
  // (plausibly set by whoever is using the very feature under test).
  if (!("OJ_DEBUG_MEM" in env)) delete childEnv.OJ_DEBUG_MEM;
  // detached: the server gets its own process group, so the kill below can
  // take the whole group — config extraction runs in an `oj engine-job`
  // child, and killing only the parent orphans it mid-write into .oj-cache
  // (the teardown then races its writes: the ENOTEMPTY class).
  const proc = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore", env: childEnv, detached: true });
  for (let i = 0; i < 200; i++) {
    try {
      if ((await fetch(`http://localhost:${port}/`)).ok) return proc;
    } catch {}
    await sleep(50);
  }
  // Never leak the child past a readiness timeout: kill and await before
  // throwing, or it holds the port for the rest of the CI job.
  await killGroup(proc);
  throw new Error("no server");
}

async function killGroup(proc) {
  try {
    process.kill(-proc.pid, "SIGKILL");
  } catch {
    proc.kill("SIGKILL");
  }
  await new Promise((r) => proc.on("exit", r));
}

let failed = false;
let proc;
try {
  // Enabled: 200 with a requested count, >=1 once the plugin host is up.
  proc = await boot(5464, { OJ_DEBUG_MEM: "1" });
  await fetch(`http://localhost:5464/src/main.js`); // nudge the plugin host awake
  let collected = 0;
  for (let i = 0; i < 40 && collected === 0; i++) {
    const res = await fetch(`http://localhost:5464/@oj/debug/gc`);
    if (res.status !== 200) throw new Error(`enabled: expected 200, got ${res.status}`);
    ({ collected } = await res.json());
    if (collected === 0) await sleep(250);
  }
  if (collected < 1) throw new Error("gc never ran in a live engine");
  // A browser page's cross-origin fetch (Origin header present) is refused:
  // the endpoint is for local probes only.
  const xorigin = await fetch(`http://localhost:5464/@oj/debug/gc`, { headers: { origin: "http://evil.example" } });
  if (xorigin.status !== 403) throw new Error(`cross-origin: expected 403, got ${xorigin.status}`);
  await killGroup(proc);

  // Disabled: the route does not exist.
  proc = await boot(5465, {});
  const res = await fetch(`http://localhost:5465/@oj/debug/gc`);
  if (res.status !== 404) throw new Error(`disabled: expected 404, got ${res.status}`);
  console.log("DEBUG GC E2E PASSED");
} catch (err) {
  failed = true;
  console.error("DEBUG GC E2E FAILED:", err.message);
} finally {
  if (proc) await killGroup(proc);
  // The SIGKILL'd server can still be flushing code-cache writes: retry the
  // teardown instead of racing it (the known ENOTEMPTY class).
  fs.rmSync(app, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
}
process.exit(failed ? 1 : 0);
