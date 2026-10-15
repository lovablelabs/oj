// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite closes the bundle in a `finally` (build.ts) so a failed build still runs
// every plugin's buildEnd (with the error) and closeBundle, and closing the dev
// server runs buildEnd then closeBundle (pluginContainer.close). oj used to skip
// both on a failed build and never ran them when `oj dev` was interrupted.
// This is the dev half (a signal and an exit code); the build half is
// crates/oj/src/build/plugin_api_tests/plugin_close_hooks.rs.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const PORT = 6411;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-closehooks-"));
const marks = fs.mkdtempSync(path.join(os.tmpdir(), "oj-closehooks-marks-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "closehooks", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "main.js"), `document.body.dataset.ok = "1";\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `import fs from "node:fs";
const log = (line) => fs.appendFileSync(${JSON.stringify(marks)} + "/events", line + "\\n");
export default [{
  name: "lifecycle",
  buildEnd(err) { log("buildEnd:" + (err ? err.message.split("\\n")[0] : "ok")); },
  closeBundle() { log("closeBundle"); },
}];\n`,
);
const events = () =>
  fs.existsSync(path.join(marks, "events"))
    ? fs.readFileSync(path.join(marks, "events"), "utf8").trim().split("\n").filter(Boolean)
    : [];

let failed = false;
let srv = null;
try {
  // Interrupting `oj dev` runs buildEnd then closeBundle before exiting.
  srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
  await waitUp(`http://localhost:${PORT}/`);
  const exited = new Promise((res) => srv.on("exit", (code, signal) => res({ code, signal })));
  srv.kill("SIGINT");
  const outcome = await Promise.race([exited, sleep(10000).then(() => null)]);
  assert.ok(outcome, "the dev server exits on SIGINT");
  assert.equal(outcome.code, 130, `exit code follows the shell convention: ${JSON.stringify(outcome)}`);
  const ev = events();
  assert.deepEqual(ev, ["buildEnd:ok", "closeBundle"], `dev close runs buildEnd then closeBundle:\n${ev.join("\n")}`);
  srv = null;
  console.log("PLUGIN-CLOSE-HOOKS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PLUGIN-CLOSE-HOOKS E2E FAILED:", err.message);
} finally {
  if (srv) srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
  fs.rmSync(marks, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
