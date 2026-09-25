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
  for (let i = 0; i < 120; i++) {
    try { if ((await fetch(BASE)).ok) return true; } catch {}
    await new Promise((r) => setTimeout(r, 250));
  }
  return false;
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
  // Rewrite .env with byte-identical content (a checkout / clone-reset shape):
  // the content-identity baseline must skip the restart and say so.
  fs.writeFileSync(path.join(app, ".env"), "VITE_FOO=1\n");
  const deduped = await (async () => {
    for (let i = 0; i < 40; i++) {
      if (/unchanged content/.test(stderr)) return true;
      if (/restarting dev server/i.test(stderr)) return false;
      await new Promise((r) => setTimeout(r, 250));
    }
    return false;
  })();
  if (!deduped) throw new Error("identical .env rewrite was not deduped:\n" + stderr);
  if (/restarting dev server/i.test(stderr)) {
    throw new Error("identical .env rewrite restarted anyway:\n" + stderr);
  }
  if (!(await fetch(BASE)).ok) throw new Error("server stopped serving after deduped rewrite");
  console.log("identical rewrite:  deduped, no restart");

  stderr = "";
  // Touch a watched config/.env file → expect a restart.
  fs.appendFileSync(path.join(app, ".env"), "VITE_BAR=2\n");

  const restarted = await (async () => {
    for (let i = 0; i < 60; i++) {
      if (/restarting dev server/i.test(stderr)) return true;
      await new Promise((r) => setTimeout(r, 250));
    }
    return false;
  })();
  if (!restarted) throw new Error("no restart log after .env change:\n" + stderr);
  console.log("restart triggered:  yes");

  if (!(await up())) throw new Error("server did not come back after restart");
  console.log("server recovered:   yes");
  console.log("\nCONFIG RESTART VERIFIED: identical rewrite deduped, real change restarts");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  child.kill("SIGKILL");
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
