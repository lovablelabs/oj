// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// The Start dev server's shutdown contract: a SIGTERM (the supervisor-restart
// shape — a sandbox restarting `oj dev` on a tanstack app) must sweep plugin
// children before the process exits, while a detached child keeps Node's
// outlive-the-parent contract. The plain-dev path has this contract covered
// by plugin-child-processes.mjs; this is the start_dev serve path, where a
// stranded child is miniflare's workerd reparented to pid 1 on every restart.
// Skips itself when the start-app fixture is not installed.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const app = path.join(here, "fixtures", "start-shutdown-app");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const PORT = 5363;

// Dependencies come from the start-app fixture install; the symlink is
// created here because gitignore swallows anything named node_modules.
const sharedDeps = path.join(here, "fixtures", "start-app", "node_modules");
if (!fs.existsSync(sharedDeps)) {
  console.log("SKIP start-shutdown-children (start-app fixture not installed)");
  process.exit(0);
}
const link = path.join(app, "node_modules");
if (!fs.existsSync(link)) {
  fs.symlinkSync(path.join("..", "start-app", "node_modules"), link);
}
const p = (rel) => path.join(app, rel);
for (const f of ["plain.pid", "detached.pid"]) fs.rmSync(p(f), { force: true });

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};
const alive = (pid) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
};
// Identity, not just liveness: a recycled pid makes bare kill(pid, 0) report
// an unrelated process as "surviving" on a busy machine.
const gone = (pid) => {
  if (!alive(pid)) return true;
  try {
    return !execSync(`ps -o comm= -p ${pid}`).toString().includes("sleep");
  } catch {
    return true;
  }
};
const reaped = (pid) => settles(() => gone(pid), { timeoutMs: 15000 });

let failed = false;
let stderr = "";
// detached: the SIGTERM must hit oj alone, never this harness's group.
const srv = spawn(oj, ["dev", ".", "--port", String(PORT)], {
  cwd: app,
  stdio: ["ignore", "ignore", "pipe"],
  detached: true,
});
srv.stderr.on("data", (d) => (stderr += d));
try {
  await waitUp(`http://localhost:${PORT}/`, { proc: srv, timeoutMs: 120000 });
  const spawned = await settles(() => fs.existsSync(p("plain.pid")) && fs.existsSync(p("detached.pid")));
  must(spawned, `plugin never spawned its children\n${stderr.slice(-2000)}`);
  const plainPid = Number(fs.readFileSync(p("plain.pid"), "utf8"));
  const detachedPid = Number(fs.readFileSync(p("detached.pid"), "utf8"));
  must(alive(plainPid) && alive(detachedPid), "both children should be running");

  srv.kill("SIGTERM");
  must(await reaped(plainPid), "plain child survived SIGTERM (start dev did not sweep children)");
  must(alive(detachedPid), "detached child must outlive SIGTERM");
  console.log("sigterm sweep:      ok");
  console.log("\nSTART SHUTDOWN CHILDREN VERIFIED");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  try {
    process.kill(-srv.pid, "SIGKILL");
  } catch {}
  try {
    srv.kill("SIGKILL");
  } catch {}
  for (const f of ["plain.pid", "detached.pid"]) {
    try {
      process.kill(Number(fs.readFileSync(p(f), "utf8")), "SIGKILL");
    } catch {}
    fs.rmSync(p(f), { force: true });
  }
}
process.exit(failed ? 1 : 0);
