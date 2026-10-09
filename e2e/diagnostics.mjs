// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// /@oj/diagnostics lists the dev server's recent errors: a module that fails
// to compile must appear as a `compile_error` event (with its path), repeats
// of the same error must fold into one entry with a bumped count, the
// `?after=` cursor must filter, and a request carrying an Origin header must
// be refused. With OJ_LOG_JSON=1 the same event must also reach stderr as one
// parseable NDJSON line marked `"oj":"diag"`.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { settles, sleep, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const port = 5399;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-diag-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "diag", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/main.tsx"></script></body></html>`,
);
fs.writeFileSync(path.join(app, "ok.tsx"), `export const fine = 1;\n`);
// A parse error oxc cannot recover: every request for it is a 500.
fs.writeFileSync(path.join(app, "main.tsx"), `export const broken = {;\n`);

const diagnostics = async (extra = "") => {
  const res = await fetch(`http://localhost:${port}/@oj/diagnostics${extra}`);
  assert.equal(res.status, 200, "diagnostics route answers");
  return res.json();
};

const proc = spawn(oj, ["dev", app, "--port", String(port)], {
  stdio: ["ignore", "ignore", "pipe"],
  env: { ...process.env, OJ_LOG_JSON: "1" },
});
let stderr = "";
proc.stderr.on("data", (d) => (stderr += d));

let failed = false;
try {
  await waitUp(`http://localhost:${port}/ok.tsx`, { proc });

  // Before any request fails there is no compile_error (this bare fixture has
  // no rolldown, so the ring legitimately holds the optimizer's own event).
  const clean = await diagnostics();
  assert.deepEqual(
    clean.events.filter((e) => e.kind === "compile_error"),
    [],
    "no compile errors before the first failing request",
  );
  assert.equal(clean.pluginHost.present, false, "no plugins in this app");
  assert.ok(Number(clean.startedAt) > 0, "startedAt is set");

  for (let i = 0; i < 3; i++) {
    const res = await fetch(`http://localhost:${port}/main.tsx`);
    assert.equal(res.status, 500, "the broken module is a 500");
  }

  const body = await diagnostics();
  const compileErrors = body.events.filter((e) => e.kind === "compile_error");
  assert.equal(compileErrors.length, 1, "identical repeats fold into one entry");
  const event = compileErrors[0];
  assert.equal(event.count, 3, "the fold counted every repeat");
  assert.equal(event.level, "error");
  assert.equal(event.source, "server");
  assert.match(event.message, /main\.tsx/, "the event names the module");
  assert.ok(Number(event.ts) > 0, "the event is stamped");
  assert.equal(body.counters.compile_error, 3, "counters see every repeat");

  // The cursor: everything is older than a stamp from the future.
  const later = await diagnostics(`?after=${Date.now() + 60_000}`);
  assert.deepEqual(later.events, [], "?after filters out older events");

  // A browser page's cross-origin fetch carries Origin: refused.
  const origin = await fetch(`http://localhost:${port}/@oj/diagnostics`, {
    headers: { Origin: "http://evil.example" },
  });
  assert.equal(origin.status, 403, "Origin-bearing requests are refused");

  // The same failure reached stderr as one NDJSON line.
  const ok = await settles(() =>
    stderr.split("\n").some((line) => {
      try {
        const v = JSON.parse(line);
        return v.oj === "diag" && v.kind === "compile_error" && /main\.tsx/.test(v.message);
      } catch {
        return false;
      }
    }),
  );
  assert.ok(ok, `stderr carries the NDJSON diag line, got:\n${stderr}`);

  console.log("PASS diagnostics");
} catch (e) {
  failed = true;
  console.error("FAIL diagnostics:", e.message);
  if (stderr) console.error("--- server stderr ---\n" + stderr);
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
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
