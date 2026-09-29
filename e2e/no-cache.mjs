// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// `oj dev --no-cache` (and OJ_NO_CACHE=1) must disable the on-disk module cache:
// serving a module writes no persistent entry, so every start recompiles. A
// control run (no flag) proves the same fixture caches normally, so the
// --no-cache result is meaningful.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const port = 5316;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-nocache-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "nocache", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/main.tsx"></script></body></html>`,
);
fs.writeFileSync(path.join(app, "main.tsx"), `export const value: number = 41 + 1;\nconsole.log(value);\n`);

// A persistent module-cache entry is a `<hash>.json` inside a two-hex shard
// directory under .oj-cache/v*/. Count those (ignores graph snapshots etc.).
function moduleCacheEntries() {
  const root = path.join(app, ".oj-cache");
  let n = 0;
  const walk = (dir) => {
    let ents;
    try {
      ents = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const e of ents) {
      const p = path.join(dir, e.name);
      if (e.isDirectory()) walk(p);
      else if (e.isFile() && e.name.endsWith(".json") && /^[0-9a-f]{2}$/.test(path.basename(dir))) n++;
    }
  };
  walk(root);
  return n;
}

// Run oj (optionally with --no-cache), request the module, give the async cache
// writer time to flush, and return how many module cache entries exist after.
async function runAndCount({ noCache }) {
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  const args = ["dev", app, "--port", String(port)];
  if (noCache) args.push("--no-cache");
  // The persistent cache is opt-in (OJ_ENABLE_CACHE / --enable-cache), so the
  // control run must enable it for "caches normally" to mean anything; the
  // --no-cache run keeps the SAME env, proving the flag beats the env.
  const proc = spawn(oj, args, { stdio: "ignore", env: { ...process.env, OJ_ENABLE_CACHE: "1" } });
  try {
    await waitUp(`http://localhost:${port}/`);
    const r = await fetch(`http://localhost:${port}/main.tsx`);
    assert.equal(r.status, 200, "module serves");
    // The cache write is async (channel -> background writer). Poll briefly so
    // the control run has a fair chance to persist before we count.
    await settles(() => moduleCacheEntries() !== 0, { timeoutMs: 3000, pollMs: 150 });
    await sleep(300);
    return moduleCacheEntries();
  } finally {
    try {
      execSync(`pkill -P ${proc.pid}`);
    } catch {}
    try {
      proc.kill("SIGKILL");
    } catch {}
    try {
      execSync(`lsof -ti:${port} -sTCP:LISTEN | xargs -r kill -9`);
    } catch {}
    await sleep(500);
  }
}

let failed = false;
try {
  const cached = await runAndCount({ noCache: false });
  assert.ok(cached > 0, `control: normal run must persist module cache entries (got ${cached})`);

  const uncached = await runAndCount({ noCache: true });
  assert.equal(uncached, 0, `--no-cache must write no module cache entries (got ${uncached})`);

  console.log(`PASS no-cache (normal wrote ${cached} entries, --no-cache wrote ${uncached})`);
} catch (e) {
  failed = true;
  console.error("FAIL no-cache:", e.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
