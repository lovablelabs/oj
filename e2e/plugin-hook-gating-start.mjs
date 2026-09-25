// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// SSR/Start hook gating: the per-plugin filter gate must skip the isolate RPC
// for server-graph modules no plugin's filter can claim. Runs the minimal
// start-gating-app fixture (tanstackStart + react + minority-filter plugins,
// node_modules shared with start-app via symlink): with no function-form
// hooks in play, both the SSR transform gate and the Start load gate must
// report skips, and the page must still render. Skips itself when the
// start-app fixture is not installed.

import { spawn } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const app = path.join(here, "fixtures", "start-gating-app");
const OJ = path.join(repo, "target", "debug", "oj");
const PORT = 5349;

// Dependencies come from the start-app fixture install; the symlink is
// created here because gitignore swallows anything named node_modules.
const sharedDeps = path.join(here, "fixtures", "start-app", "node_modules");
if (!fs.existsSync(sharedDeps)) {
  console.log("SKIP plugin-hook-gating-start (start-app fixture not installed)");
  process.exit(0);
}
const link = path.join(app, "node_modules");
if (!fs.existsSync(link)) {
  fs.symlinkSync(path.join("..", "start-app", "node_modules"), link);
}

let failed = false;
let child;
try {
  let stderr = "";
  child = spawn(OJ, ["dev", ".", "--port", String(PORT)], {
    cwd: app,
    stdio: ["ignore", "ignore", "pipe"],
    detached: true,
    env: { ...process.env, OJ_DEBUG_HOOK_GATE: "1" },
  });
  child.stderr.on("data", (d) => (stderr += d.toString()));
  const t0 = Date.now();
  let body = "";
  for (;;) {
    try {
      const res = await fetch(`http://localhost:${PORT}/`);
      if (res.ok) {
        body = await res.text();
        break;
      }
    } catch {}
    if (Date.now() - t0 > 120000) throw new Error(`start dev did not render:\n${stderr.slice(-2000)}`);
    await new Promise((r) => setTimeout(r, 250));
  }

  assert.match(body, /data-done/, "the SSR page rendered");
  assert.match(body, /plain-module-crossed/, "an ordinary server module rendered through the gate");

  // Both gates must have skipped RPCs for unclaimed server modules.
  const tRe = /hook gate skipped ssr transform for /;
  const lRe = /hook gate skipped start load for /;
  for (const t1 = Date.now(); !(tRe.test(stderr) && lRe.test(stderr)) && Date.now() - t1 < 5000; ) {
    await new Promise((r) => setTimeout(r, 50));
  }
  assert.match(stderr, tRe, "ssr transform gate skipped unclaimed server modules");
  assert.match(stderr, lRe, "start load gate skipped unclaimed server modules");

  console.log("PLUGIN HOOK GATING START VERIFIED");
} catch (e) {
  failed = true;
  console.error(e);
} finally {
  if (child) {
    try {
      process.kill(-child.pid, "SIGKILL");
    } catch {}
    try {
      child.kill("SIGKILL");
    } catch {}
    await new Promise((r) => {
      child.once("exit", r);
      setTimeout(r, 5000);
    });
  }
}
process.exit(failed ? 1 : 0);
