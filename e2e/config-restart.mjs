// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Verifies the dev server restarts itself when a config/.env file changes
// (config is read once at startup, so it can't be hot-applied). Standalone:
// manages its own oj process because the restart re-execs it.

import { spawn } from "node:child_process";
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
  // Touch a watched config/.env file → expect a restart.
  fs.appendFileSync(path.join(app, ".env"), "VITE_BAR=2\n");

  const restarted = await settles(() => /restarting dev server/i.test(stderr));
  if (!restarted) throw new Error("no restart log after .env change:\n" + stderr);
  console.log("restart triggered:  yes");

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
