// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Verifies the dev server restarts itself when a config/.env file changes
// (config is read once at startup, so it can't be hot-applied). Standalone:
// manages its own oj process because the restart re-execs it.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { settles, waitUp } from "./util.mjs";

const OJ = path.join(process.cwd(), "target", "debug", "oj");
const PORT = 5251;
const BASE = `http://localhost:${PORT}/`;

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-restart-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), '{"name":"restart-fixture","private":true}');
fs.writeFileSync(
  path.join(app, "index.html"),
  '<!doctype html><html><head></head><body><script type="module" src="/src/main.tsx"></script></body></html>',
);
fs.writeFileSync(path.join(app, "src", "main.tsx"), 'document.body.dataset.ok = "1";\n');
fs.writeFileSync(path.join(app, ".env"), "VITE_FOO=1\n");
// A plugin-spawned long-lived child (the workerd shape): the restart assert
// below proves the self-restart reaps it.
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `import { spawn } from "node:child_process";
import { writeFileSync } from "node:fs";
export default [{
  name: "test:child-spawner",
  configureServer() {
    // Not process.execPath: inside oj's embedded engine that is the oj
    // binary itself, which exits instantly on -e.
    const child = spawn("sleep", ["300"], { stdio: "ignore" });
    writeFileSync(new URL("./child.pid", import.meta.url), String(child.pid));
  },
}];\n`,
);

const up = async () => {
  return waitUp(BASE).then(
    () => true,
    () => false,
  );
};

let stderr = "";
const child = spawn(OJ, ["dev", "--port", String(PORT)], { cwd: app });
child.stderr.on("data", (d) => (stderr += d.toString()));
child.stdout.on("data", () => {});

let failed = false;
try {
  if (!(await up())) throw new Error("server did not start");
  console.log("initial start:      ok");

  stderr = "";
  // Read the FIRST boot's child pid before triggering the restart: a fast
  // reboot re-runs configureServer and overwrites child.pid with the new
  // boot's healthy child.
  const childPid = Number(fs.readFileSync(path.join(app, "child.pid"), "utf8"));
  // Touch a watched config/.env file → expect a restart.
  fs.appendFileSync(path.join(app, ".env"), "VITE_BAR=2\n");

  const restarted = await settles(() => /restarting dev server/i.test(stderr));
  if (!restarted) throw new Error("no restart log after .env change:\n" + stderr);
  console.log("restart triggered:  yes");

  // The self-restart is an exec: same pid, sockets closed via CLOEXEC — but a
  // plugin-spawned runtime (miniflare's workerd) used to SURVIVE it as a
  // stranded frozen child holding its whole footprint. restart_process() now
  // kills every descendant first.
  // Identity, not just liveness: a recycled pid makes bare kill(pid, 0) report
  // an unrelated process as the "surviving" child on a busy machine.
  const childDead = await settles(
    () => {
      try {
        process.kill(childPid, 0);
      } catch {
        return true;
      }
      try {
        return !execSync(`ps -o comm= -p ${childPid}`).toString().includes("sleep");
      } catch {
        return true;
      }
    },
    { timeoutMs: 15000 },
  );
  if (!childDead) {
    try {
      process.kill(childPid, "SIGKILL");
    } catch {}
    throw new Error(`plugin-spawned child ${childPid} survived the self-restart`);
  }
  console.log("descendants killed: yes");

  if (!(await up())) throw new Error("server did not come back after restart");
  console.log("server recovered:   yes");
  console.log("\nCONFIG RESTART VERIFIED: .env change restarts the dev server");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  child.kill("SIGKILL");
  // The restarted server's short-lived children may still be flushing into
  // the app dir (the suite's ENOTEMPTY teardown class): retry the removal.
  await settles(
    () => {
      try {
        fs.rmSync(app, { recursive: true, force: true });
        return true;
      } catch {
        return false;
      }
    },
    { timeoutMs: 5000, pollMs: 150 },
  );
}
process.exit(failed ? 1 : 0);
