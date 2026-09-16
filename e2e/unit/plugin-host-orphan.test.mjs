// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";
import { asset, tmpProject } from "./harness.mjs";

// The stdin-EOF net arms only after the host's top-level awaits. A host whose
// boot is stuck in an await its dead parent will never settle (an RPC reply, a
// wedged plugin import) writes nothing (no EPIPE crash) and reads nothing (no
// EOF), so before the ppid watchdog it survived as an orphan forever — 40 of
// them, one per graded boot, after a kill-heavy test session. The wedged
// plugins file models that window deterministically: the import never
// finishes, and an interval keeps the event loop alive the way the bridge
// fifos and middleware server do in a real boot.
const wedgedPlugins = `
setInterval(() => {}, 60_000);
await new Promise(() => {});
export default [];
`;

const alive = (pid) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
};

test("an orphaned plugin host stuck in a boot await reaps itself (ppid watchdog)", async () => {
  const fx = tmpProject({ prefix: "oj-orphan-" });
  fx.write("oj.plugins.mjs", wedgedPlugins);
  // An intermediate parent stands in for the oj process: it spawns the host,
  // reports the pid, and idles until it is SIGKILLed (a crashed / kill -9'd oj
  // closes no descriptors gracefully, exactly like this).
  const parentScript = `
    const { spawn } = require("node:child_process");
    const c = spawn(process.execPath, [process.argv[1], process.argv[2], process.argv[3]], { stdio: ["pipe", "pipe", "pipe"] });
    console.log("HOST=" + c.pid);
    setInterval(() => {}, 60_000);
  `;
  const parent = spawn(process.execPath, [
    "-e",
    parentScript,
    asset("plugin-host.mjs"),
    path.join(fx.root, "oj.plugins.mjs"),
    JSON.stringify({ config: { root: fx.root } }),
  ], { cwd: fx.root, env: { ...process.env, OJ_CACHE_ROOT: fx.root }, stdio: ["ignore", "pipe", "pipe"] });
  try {
    const hostPid = await new Promise((resolve, reject) => {
      let buf = "";
      parent.stdout.on("data", (d) => {
        buf += d;
        const m = buf.match(/HOST=(\d+)/);
        if (m) resolve(Number(m[1]));
      });
      parent.on("exit", () => reject(new Error(`parent exited before reporting the host pid: ${buf}`)));
    });
    // Give the host time to boot into the wedged import, and prove the wedge
    // holds: the host must still be running while its parent lives.
    await sleep(1500);
    assert.ok(alive(hostPid), "the wedged host stays alive under a living parent");
    parent.kill("SIGKILL");
    // The watchdog polls every second; the orphan must be gone well within 5s.
    let gone = false;
    for (let i = 0; i < 25 && !(gone = !alive(hostPid)); i++) await sleep(200);
    assert.ok(gone, `orphaned host ${hostPid} must reap itself after its parent dies`);
  } finally {
    parent.kill("SIGKILL");
    fx.cleanup();
  }
});
