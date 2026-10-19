// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { execFileSync, execSync, spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import { tmpProject } from "./harness.mjs";

const repo = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const oj = path.join(repo, "target", "debug", "oj");
if (!fs.existsSync(oj)) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
execFileSync(oj, ["--version"], { stdio: "ignore" });

// The dev server spawns the dep optimizer's job when the scan begins and
// sends the scan result later: `--prepare` runs its export on the first stdin
// line, before the job payload is read, in the same isolate as the job.
test("an engine-job child runs --prepare before its payload arrives", async () => {
  const fx = tmpProject({ prefix: "oj-engine-job-prepare-" });
  fx.write("package.json", JSON.stringify({ name: "prepare-fx", version: "1.0.0", type: "module" }));
  fx.write(
    "job.mjs",
    `import { writeFileSync } from "node:fs";
let prepared = null;
export function prepare(input) {
  prepared = input.tag;
  writeFileSync(input.marker, "");
  return null;
}
export function run(payload) {
  return { prepared, payload };
}
`,
  );
  const marker = path.join(fx.root, "prepared");
  const result = path.join(fx.root, "result.json");
  const child = spawn(
    oj,
    [
      "engine-job",
      path.join(fx.root, "job.mjs"),
      "--root",
      fx.root,
      "--export",
      "run",
      "--timeout-secs",
      "60",
      "--result",
      result,
      "--prepare",
      "prepare",
    ],
    { cwd: fx.root, stdio: ["pipe", "ignore", "inherit"], env: { ...process.env, OJ_PARENT_PID: String(process.pid) } },
  );
  try {
    child.stdin.write(JSON.stringify({ tag: "early", marker }) + "\n");
    for (let i = 0; i < 300 && !fs.existsSync(marker); i++) await sleep(50);
    assert.ok(fs.existsSync(marker), "prepare ran while the payload was still unsent");
    assert.ok(!fs.existsSync(result), "the job itself waits for its payload");
    const exited = new Promise((resolve) => child.once("exit", resolve));
    child.stdin.end(JSON.stringify({ n: 1 }));
    assert.equal(await exited, 0);
    const envelope = JSON.parse(fs.readFileSync(result, "utf8"));
    assert.deepEqual(envelope, { ok: true, value: { prepared: "early", payload: { n: 1 } } });
  } finally {
    child.kill("SIGKILL");
    fx.cleanup();
  }
});
