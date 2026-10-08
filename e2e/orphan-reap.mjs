// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// The two child-stranding paths no in-process kill sweep covers:
// 1. A signal DURING boot: the shutdown listener must already be installed
//    (Vite installs it inside createServer, before configureServer hooks),
//    so a SIGTERM while a plugin is still blocking boot kills its children.
// 2. SIGKILL of oj itself (OOM kill, supervisor escalation): no handler runs,
//    children orphan to pid 1; the NEXT boot must sweep them from the
//    persisted child registry.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const PORT = 5365;

const alive = (pid) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
};

function makeApp(name, plugin) {
  const app = fs.mkdtempSync(path.join(os.tmpdir(), `oj-orphan-${name}-`));
  fs.writeFileSync(
    path.join(app, "package.json"),
    JSON.stringify({ name: `orphan-${name}`, private: true, type: "module" }),
  );
  fs.writeFileSync(path.join(app, "index.html"), "<!doctype html><html><body>OK</body></html>");
  fs.writeFileSync(path.join(app, "oj.plugins.mjs"), plugin);
  return app;
}

function startOj(app) {
  const srv = spawn(oj, ["dev", app, "--port", String(PORT)], {
    cwd: app,
    stdio: ["ignore", "pipe", "pipe"],
  });
  srv.log = "";
  srv.stdout.on("data", (d) => (srv.log += d));
  srv.stderr.on("data", (d) => (srv.log += d));
  return srv;
}

const childPid = (srv) => Number(srv.log.match(/CHILD (\d+)/)?.[1]);
const exited = (srv) => new Promise((r) => (srv.exitCode !== null ? r() : srv.once("exit", r)));
const sleeperPlugin = (blockBoot) => `import { spawn } from "node:child_process";
export default [{
  name: "test:sleeper",
  async configureServer() {
    const child = spawn("node", ["-e", "setInterval(() => {}, 1000)"], { stdio: "ignore" });
    console.error("CHILD " + child.pid);
    ${blockBoot ? "await new Promise(() => {});" : ""}
  },
}];
`;

let failed = false;
try {
  // --- 1: SIGTERM while configureServer still blocks boot ---
  const appA = makeApp("boot", sleeperPlugin(true));
  const srvA = startOj(appA);
  if (!(await settles(() => childPid(srvA), { timeoutMs: 60000 })))
    throw new Error(`plugin never spawned its child:\n${srvA.log}`);
  const pidA = childPid(srvA);
  // Liveness gate first: a child that died on its own makes the kill assert vacuous.
  if (!alive(pidA)) throw new Error(`plugin child ${pidA} was never alive:\n${srvA.log}`);
  srvA.kill("SIGTERM");
  await exited(srvA);
  if (!(await settles(() => !alive(pidA), { timeoutMs: 10000 })))
    throw new Error(`mid-boot SIGTERM left the plugin child ${pidA} running:\n${srvA.log}`);
  fs.rmSync(appA, { recursive: true, force: true });
  console.log("mid-boot SIGTERM kills plugin children");

  // --- 2: SIGKILL orphans the child; the next boot reaps it ---
  const appB = makeApp("reap", sleeperPlugin(false));
  const srvB = startOj(appB);
  await waitUp(`http://localhost:${PORT}/`, { proc: srvB });
  const pidB = childPid(srvB);
  if (!pidB) throw new Error(`no child pid in log:\n${srvB.log}`);
  srvB.kill("SIGKILL");
  await exited(srvB);
  if (!alive(pidB)) throw new Error("child died with oj; nothing left to prove reaping on");

  const srvC = startOj(appB);
  try {
    await waitUp(`http://localhost:${PORT}/`, { proc: srvC });
    if (!(await settles(() => !alive(pidB), { timeoutMs: 10000 })))
      throw new Error(`restarted oj never reaped orphan ${pidB}:\n${srvC.log}`);
    if (!/reaped \d+ orphaned child/.test(srvC.log))
      throw new Error(`no reap line in the new server's log:\n${srvC.log}`);
  } finally {
    srvC.kill("SIGKILL");
    await exited(srvC);
    if (childPid(srvC)) {
      try {
        process.kill(childPid(srvC), "SIGKILL");
      } catch {}
    }
  }
  fs.rmSync(appB, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
  console.log("next boot reaps the SIGKILL orphan");
  console.log("ORPHAN-REAP E2E PASSED");
} catch (err) {
  failed = true;
  console.error(`ORPHAN-REAP E2E FAILED: ${err.message}`);
}
process.exit(failed ? 1 : 0);
