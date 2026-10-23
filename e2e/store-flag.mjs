// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// `oj dev --store[=DIR]` is the one switch for the oj store: it enables the
// persistent module cache and (with DIR) relocates the shared store. This
// proves the flag alone persists modules into the given store, that the
// entries are relocatable (nothing lands in the app's own cache), and that
// `--no-cache` still wins over it.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const port = 5321;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-storeflag-"));
const store = fs.mkdtempSync(path.join(os.tmpdir(), "oj-storeflag-store-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "sf", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/main.tsx"></script></body></html>`,
);
fs.writeFileSync(path.join(app, "main.tsx"), `export const value: number = 41 + 1;\nconsole.log(value);\n`);

// The flag must work with NO env help: strip the store var.
const env = { ...process.env };
delete env.OJ_STORE_DIR;

function entriesUnder(root) {
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
const storeEntries = () => entriesUnder(path.join(store, "v1", "modules"));
const appEntries = () => entriesUnder(path.join(app, ".oj-cache"));

async function runAndCount(extraArgs) {
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  fs.rmSync(path.join(store, "v1"), { recursive: true, force: true });
  const proc = spawn(oj, ["dev", app, "--port", String(port), `--store=${store}`, ...extraArgs], {
    stdio: "ignore",
    env,
  });
  try {
    let up = false;
    for (let i = 0; i < 120 && !up; i++) {
      try {
        up = (await fetch(`http://localhost:${port}/`)).ok;
      } catch {}
      if (!up) await sleep(100);
    }
    assert.ok(up, "oj never came up");
    const r = await fetch(`http://localhost:${port}/main.tsx`);
    assert.equal(r.status, 200, "module serves");
    for (let i = 0; i < 20 && storeEntries() === 0; i++) await sleep(150);
    await sleep(300);
    return { store: storeEntries(), app: appEntries() };
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
  const withStore = await runAndCount([]);
  assert.ok(withStore.store > 0, `--store=DIR must persist module entries into DIR (got ${withStore.store})`);
  assert.equal(withStore.app, 0, `relocatable entries belong to the store, not the app (got ${withStore.app})`);

  const noCache = await runAndCount(["--no-cache"]);
  assert.equal(
    noCache.store + noCache.app,
    0,
    `--no-cache must beat --store (got ${noCache.store + noCache.app} entries)`,
  );

  console.log(
    `PASS store-flag (--store wrote ${withStore.store} entries to the store, 0 to the app; --no-cache wrote 0)`,
  );
} catch (err) {
  failed = true;
  console.error(`FAIL store-flag: ${err.message}`);
} finally {
  process.exit(failed ? 1 : 0);
}
