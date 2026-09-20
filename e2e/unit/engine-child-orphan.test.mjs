// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { execSync, spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import { tmpProject } from "./harness.mjs";

const repo = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const oj = path.join(repo, "target", "debug", "oj");
if (!fs.existsSync(oj)) {
  // The unit-test CI job runs `cargo test --workspace` first, so this is an
  // incremental link of the bin, not a cold build.
  execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
}

const alive = (pid) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
};

// A SIGKILLed dev server runs no drop, so `kill_on_drop` never reaches its
// one-shot children (`oj start-script` / `oj engine-job`) — before the ppid
// reaper an orphan kept running its job, writing code caches into the app's
// .oj-cache for minutes past the parent's death (the recurring ENOTEMPTY in
// e2e teardowns, and stale writers after a real crash). The forever-awaiting
// script models a long job; the reaper must notice the reparent and exit.
// The birth window: a parent SIGKILLed between the spawn and the child's
// ppid snapshot has already reparented the child, so change-detection alone
// never fires. The spawner announces its pid via OJ_PARENT_PID; a child whose
// observed ppid differs at arm time was born an orphan and exits at once.
test("a child born an orphan exits immediately (OJ_PARENT_PID mismatch)", async () => {
  const fx = tmpProject({ prefix: "oj-born-orphan-" });
  fx.write("package.json", JSON.stringify({ name: "orphan-fx", version: "1.0.0" }));
  fx.write("forever.mjs", "setInterval(() => {}, 60_000);\nawait new Promise(() => {});\n");
  // Announce a parent that is not this process: to the child that reads as
  // "my spawner is already gone".
  const child = spawn(oj, ["start-script", path.join(fx.root, "forever.mjs"), "--root", fx.root], {
    cwd: fx.root,
    env: { ...process.env, OJ_PARENT_PID: "1" },
    stdio: ["pipe", "ignore", "ignore"],
  });
  child.stdin.end(JSON.stringify([]));
  try {
    const code = await new Promise((resolve, reject) => {
      child.on("exit", (c) => resolve(c));
      setTimeout(() => reject(new Error("born-orphan child did not exit")), 5000);
    });
    assert.equal(code, 0, "a born orphan exits cleanly instead of running its job");
  } finally {
    child.kill("SIGKILL");
    fx.cleanup();
  }
});

test("an orphaned start-script child reaps itself when its parent dies", async () => {
  const fx = tmpProject({ prefix: "oj-child-orphan-" });
  fx.write("package.json", JSON.stringify({ name: "orphan-fx", version: "1.0.0" }));
  // The interval keeps the event loop alive so the await is a legitimately
  // pending long job (an idle loop with a never-settling top-level await is
  // failed by the engine as "never resolved" instead).
  fx.write("forever.mjs", "setInterval(() => {}, 60_000);\nawait new Promise(() => {});\n");
  // An intermediate parent stands in for the oj dev server: it spawns the
  // child, feeds the stdin env payload, reports the pid, and idles until it
  // is SIGKILLed.
  const parentScript = `
    const { spawn } = require("node:child_process");
    // Every real spawner declares its pid (OJ_PARENT_PID) so a child that is
    // still loading when the parent dies still catches the reparent.
    const c = spawn(process.argv[1], ["start-script", process.argv[2], "--root", process.argv[3]], { stdio: ["pipe", "ignore", "ignore"], env: { ...process.env, OJ_PARENT_PID: String(process.pid) } });
    c.stdin.end(JSON.stringify([]));
    console.log("CHILD=" + c.pid);
    setInterval(() => {}, 60_000);
  `;
  const parent = spawn(
    process.execPath,
    ["-e", parentScript, oj, path.join(fx.root, "forever.mjs"), fx.root],
    { cwd: fx.root, stdio: ["ignore", "pipe", "pipe"] },
  );
  try {
    const childPid = await new Promise((resolve, reject) => {
      let buf = "";
      parent.stdout.on("data", (d) => {
        buf += d;
        const m = buf.match(/CHILD=(\d+)/);
        if (m) resolve(Number(m[1]));
      });
      parent.on("exit", () => reject(new Error("intermediate parent died early")));
      setTimeout(() => reject(new Error("no child pid reported")), 15_000);
    });
    // The child is up and parked in its forever job.
    await sleep(500);
    assert.ok(alive(childPid), "the child runs while its parent lives");

    parent.kill("SIGKILL");
    // The reaper polls every 200ms; give it a couple of seconds.
    let gone = false;
    for (let i = 0; i < 25; i++) {
      await sleep(200);
      if (!alive(childPid)) {
        gone = true;
        break;
      }
    }
    if (!gone) process.kill(childPid, "SIGKILL");
    assert.ok(gone, "the orphaned child exits on the reparent instead of running out its job");
  } finally {
    parent.kill("SIGKILL");
    fx.cleanup();
  }
});

// The reaper compares the live ppid against the pid the SPAWNER declared in
// OJ_PARENT_PID, not against a snapshot taken once the binary is up: a parent
// that died while the child was still loading has already reparented it, and
// a snapshot would never change. A declared pid that is not the real parent
// models exactly that state; the child must exit on the first poll, before it
// boots an engine or writes a code cache.
test("an engine-job child whose declared parent is already gone exits before running its job", async () => {
  const fx = tmpProject({ prefix: "oj-child-declared-parent-" });
  fx.write("package.json", JSON.stringify({ name: "declared-fx", version: "1.0.0" }));
  fx.write("forever.mjs", "export async function run() { setInterval(() => {}, 60_000); await new Promise(() => {}); }\n");
  const result = path.join(fx.root, "result.json");
  // pid_t max: never a live process, so never this child's real parent.
  const child = spawn(
    oj,
    ["engine-job", path.join(fx.root, "forever.mjs"), "--root", fx.root, "--export", "run", "--timeout-secs", "60", "--result", result],
    { cwd: fx.root, stdio: ["pipe", "ignore", "ignore"], env: { ...process.env, OJ_PARENT_PID: "2147483647" } },
  );
  child.stdin.end("{}");
  try {
    const exited = await Promise.race([
      new Promise((resolve) => child.once("exit", (code) => resolve({ code }))),
      sleep(5_000).then(() => null),
    ]);
    if (!exited) child.kill("SIGKILL");
    assert.ok(exited, "the child exits on its first ppid poll instead of running the forever job");
    assert.equal(exited.code, 0);
    assert.ok(!fs.existsSync(result), "no result is written for a parent that is gone");
    assert.ok(!fs.existsSync(path.join(fx.root, ".oj-cache", "v1", "code-cache")), "no code cache is written before the reaper fires");
  } finally {
    fx.cleanup();
  }
});
