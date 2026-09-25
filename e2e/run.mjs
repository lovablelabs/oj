// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const playground = path.join(repo, "playground");

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
fs.rmSync(path.join(playground, ".oj-cache"), { recursive: true, force: true });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function killPort() {
  try {
    execSync("lsof -ti:5199 -sTCP:LISTEN | xargs kill", { shell: "/bin/bash", stdio: "ignore" });
  } catch {}
}

async function startServer(logFd) {
  killPort();
  await sleep(300);
  const args = ["dev", playground, "--port", "5199"];
  const server = spawn(oj, args, { stdio: ["ignore", logFd, logFd] });
  for (let i = 0; i < 80; i++) {
    try {
      if ((await fetch("http://localhost:5199/")).ok) return server;
    } catch {}
    await sleep(250);
  }
  server.kill("SIGKILL");
  throw new Error("dev server did not start");
}

let failed = 0;
for (const file of fs.readdirSync(here).filter((f) => f.endsWith(".test.js")).sort()) {
  process.stdout.write(`\n${file}\n`);
  const logPath = path.join(here, `.server-${file}.log`);
  const logFd = fs.openSync(logPath, "w");
  let server;
  let ok = true;
  try {
    server = await startServer(logFd);
    execSync(`node ${path.join(here, file)}`, {
      stdio: "inherit",
      cwd: here,
    });
  } catch {
    failed++;
    ok = false;
  } finally {
    if (server) server.kill("SIGKILL");
    killPort();
    fs.closeSync(logFd);
    if (!ok) {
      const lines = fs.readFileSync(logPath, "utf8").trimEnd().split("\n");
      console.log(`server log (last 60 lines) for ${file}:`);
      console.log(lines.slice(-60).join("\n"));
    }
    fs.rmSync(logPath, { force: true });
  }
}
console.log(failed ? `\n${failed} test(s) FAILED` : "\nALL E2E TESTS PASSED");
process.exit(failed ? 1 : 0);
