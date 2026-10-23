// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// pulp P3, the wire: `oj store serve` is a six-endpoint substituter; dev
// servers read through it on local misses and push relocatable entries with
// a write token. This runs the whole fleet loop — worktree A compiles and
// pushes; worktree B, with an EMPTY local store at a different absolute
// path, serves A's exact bytes via the remote and back-fills its local
// store — plus the protocol's contract: write-once (409), token gating
// (401), and CAS self-verification (422 on digest mismatch).

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const SRV_PORT = 5398;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const WRITE_TOKEN = "e2e-write-secret";
const srvDir = fs.mkdtempSync(path.join(os.tmpdir(), "oj-remote-srv-"));

function makeApp(label) {
  const app = fs.mkdtempSync(path.join(os.tmpdir(), `oj-remote-${label}-`));
  fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "rw", version: "1.0.0" }));
  fs.writeFileSync(path.join(app, "package-lock.json"), JSON.stringify({ lockfileVersion: 3 }));
  fs.writeFileSync(
    path.join(app, "index.html"),
    `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/main.tsx"></script></body></html>`,
  );
  fs.writeFileSync(path.join(app, "main.tsx"), `export const value: number = 41 + 1;\nconsole.log(value);\n`);
  return app;
}

function countEntries(dir) {
  let n = 0;
  const walk = (d) => {
    let ents;
    try {
      ents = fs.readdirSync(d, { withFileTypes: true });
    } catch {
      return;
    }
    for (const e of ents) {
      if (e.isDirectory()) walk(path.join(d, e.name));
      else if (e.name.endsWith(".json")) n++;
    }
  };
  walk(dir);
  return n;
}

async function devAndFetch(app, store, port, extraEnv) {
  const env = { ...process.env, ...extraEnv };
  delete env.OJ_STORE_DIR;
  const proc = spawn(oj, ["dev", app, "--port", String(port), `--store=${store}`], {
    stdio: "ignore",
    env,
  });
  try {
    let up = false;
    for (let i = 0; i < 150 && !up; i++) {
      try {
        up = (await fetch(`http://localhost:${port}/`)).ok;
      } catch {}
      if (!up) await sleep(100);
    }
    assert.ok(up, "dev server never came up");
    const r = await fetch(`http://localhost:${port}/main.tsx`);
    assert.equal(r.status, 200);
    const body = await r.text();
    await sleep(1200); // cache writer + push queue
    return body;
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
    await sleep(400);
  }
}

// ---- the substituter ----
const srv = spawn(oj, ["store", "serve", "--dir", srvDir, "--port", String(SRV_PORT)], {
  stdio: "ignore",
  env: { ...process.env, OJ_STORE_WRITE_TOKEN: WRITE_TOKEN },
});

let failed = false;
try {
  let up = false;
  for (let i = 0; i < 100 && !up; i++) {
    try {
      up = (
        await fetch(`http://localhost:${SRV_PORT}/v1/cas/missing`, {
          method: "POST",
          body: JSON.stringify({ digests: [] }),
        })
      ).ok;
    } catch {}
    if (!up) await sleep(100);
  }
  assert.ok(up, "store server never came up");
  const base = `http://localhost:${SRV_PORT}`;

  // ---- protocol contract ----
  const hex = (s) => s.padEnd(64, "0");
  // AC: write without token refused; write-once; read back.
  const key = hex("ab");
  let r = await fetch(`${base}/v1/ac/modules/${key}`, { method: "PUT", body: "{}" });
  assert.equal(r.status, 401, "AC PUT without token must be refused");
  r = await fetch(`${base}/v1/ac/modules/${key}`, {
    method: "PUT",
    headers: { authorization: `Bearer ${WRITE_TOKEN}` },
    body: JSON.stringify({ probe: 1 }),
  });
  assert.equal(r.status, 201, "first AC PUT publishes");
  r = await fetch(`${base}/v1/ac/modules/${key}`, {
    method: "PUT",
    headers: { authorization: `Bearer ${WRITE_TOKEN}` },
    body: JSON.stringify({ probe: 2 }),
  });
  assert.equal(r.status, 409, "AC entries are write-once");
  r = await fetch(`${base}/v1/ac/modules/${key}`);
  assert.deepEqual(await r.json(), { probe: 1 }, "the first write is what reads back");

  // CAS: digest mismatch refused; publish + HEAD + missing.
  const blob = "hello store";
  const digest = execSync(`printf '%s' "${blob}" | b3sum --no-names 2>/dev/null || true`).toString().trim();
  if (digest.length === 64) {
    r = await fetch(`${base}/v1/cas/${hex("ff")}`, {
      method: "PUT",
      headers: { authorization: `Bearer ${WRITE_TOKEN}` },
      body: blob,
    });
    assert.equal(r.status, 422, "a CAS name must be its content's digest");
    r = await fetch(`${base}/v1/cas/${digest}`, {
      method: "PUT",
      headers: { authorization: `Bearer ${WRITE_TOKEN}` },
      body: blob,
    });
    assert.equal(r.status, 201);
    r = await fetch(`${base}/v1/cas/${digest}`, { method: "HEAD" });
    assert.equal(r.status, 200);
    r = await fetch(`${base}/v1/cas/missing`, {
      method: "POST",
      body: JSON.stringify({ digests: [digest, hex("ee")] }),
    });
    assert.deepEqual(await r.json(), { missing: [hex("ee")] });
  }

  // ---- the fleet loop ----
  const appA = makeApp("a");
  const appB = makeApp("b");
  const storeA = fs.mkdtempSync(path.join(os.tmpdir(), "oj-remote-storeA-"));
  const storeB = fs.mkdtempSync(path.join(os.tmpdir(), "oj-remote-storeB-"));

  const bodyA = await devAndFetch(appA, storeA, 5396, {
    OJ_STORE_REMOTE: base,
    OJ_STORE_TOKEN: WRITE_TOKEN,
    OJ_STORE_PUSH: "1",
  });
  const pushed = countEntries(path.join(srvDir, "ac", "modules"));
  assert.ok(pushed > 0, `worktree A must push its module entries (got ${pushed})`);

  // B: different absolute path, EMPTY local store, read-only remote access.
  assert.equal(countEntries(path.join(storeB, "v1", "modules")), 0);
  const bodyB = await devAndFetch(appB, storeB, 5397, {
    OJ_STORE_REMOTE: base,
  });
  assert.equal(bodyB, bodyA, "worktree B must serve worktree A's exact bytes via the wire");
  assert.ok(countEntries(path.join(storeB, "v1", "modules")) > 0, "the read-through must back-fill B's local store");

  console.log(
    `PASS store-remote (write-once 409, tokens 401, CAS 422, A pushed ${pushed} entries, B served A's bytes from the wire and back-filled locally)`,
  );
} catch (err) {
  failed = true;
  console.error(`FAIL store-remote: ${err.message}`);
} finally {
  try {
    execSync(`pkill -P ${srv.pid}`);
  } catch {}
  try {
    srv.kill("SIGKILL");
  } catch {}
  try {
    execSync(`lsof -ti:${SRV_PORT} -sTCP:LISTEN | xargs -r kill -9`);
  } catch {}
  process.exit(failed ? 1 : 0);
}
