// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// A plugin that throws in a lifecycle hook (buildStart, renderStart,
// generateBundle, writeBundle, closeBundle, buildEnd) rejects the whole build in
// Vite (rolldown rejects on hook errors): the process exits non-zero and the
// error names the plugin. In dev, Vite awaits the client buildStart while the
// server inits, so a rejection fails startup instead of serving. Logging and
// carrying on leaves a partial dist with exit code 0.

import { spawn, execSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const PORT = 6400;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-lifecycle-err-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "lifecycle-err", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "main.js"), `window.__ok = 1;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

function usePlugin(hook) {
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  fs.rmSync(path.join(app, "dist"), { recursive: true, force: true });
  fs.writeFileSync(
    path.join(app, "oj.plugins.mjs"),
    `export default [{
      name: "boom-${hook}",
      ${hook}() { throw new Error("${hook} exploded"); },
    }];\n`,
  );
}

let failed = false;
let srv = null;
try {
  for (const hook of ["buildStart", "renderStart", "generateBundle", "writeBundle", "closeBundle", "buildEnd"]) {
    usePlugin(hook);
    const r = spawnSync(oj, ["build", app], { encoding: "utf8" });
    const out = r.stderr + r.stdout;
    assert.notEqual(r.status, 0, `build must exit non-zero when ${hook} throws:\n${out}`);
    assert.match(
      out,
      new RegExp(`\\[plugin:boom-${hook}\\] ${hook} exploded`),
      `${hook} error names the plugin:\n${out}`,
    );
  }

  // A plugin whose hooks all succeed still builds (the error path is not a
  // blanket failure).
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  fs.writeFileSync(
    path.join(app, "oj.plugins.mjs"),
    `export default [{ name: "fine", buildStart() {}, closeBundle() {} }];\n`,
  );
  const ok = spawnSync(oj, ["build", app], { encoding: "utf8" });
  assert.equal(ok.status, 0, `healthy lifecycle hooks build fine:\n${ok.stderr}`);

  // Dev: a rejecting buildStart fails server startup (Vite awaits it in initServer).
  usePlugin("buildStart");
  srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
  let devOut = "";
  srv.stdout.on("data", (d) => (devOut += d));
  srv.stderr.on("data", (d) => (devOut += d));
  const exit = await new Promise((resolve) => {
    const t = setTimeout(() => resolve(null), 20000);
    srv.on("exit", (code) => {
      clearTimeout(t);
      resolve(code);
    });
  });
  assert.notEqual(exit, null, `dev must exit when buildStart throws, still running after 20s:\n${devOut}`);
  assert.notEqual(exit, 0, `dev must exit non-zero when buildStart throws:\n${devOut}`);
  assert.match(devOut, /\[plugin:boom-buildStart\] buildStart exploded/, `dev error names the plugin:\n${devOut}`);
  let served = false;
  try {
    served = (await fetch(`http://localhost:${PORT}/`)).ok;
  } catch {}
  assert.equal(served, false, "no server is left listening after a failed buildStart");

  console.log("PLUGIN-LIFECYCLE-ERRORS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PLUGIN-LIFECYCLE-ERRORS E2E FAILED:", err.message);
} finally {
  if (srv) srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
