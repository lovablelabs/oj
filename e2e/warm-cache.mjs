// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const playground = path.join(repo, "playground");
const WATCHED = path.join(playground, "plugin-watched.txt");
const PORT = 5197;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
function killPort() {
  try {
    execSync(`lsof -ti:${PORT} -sTCP:LISTEN | xargs kill`, { shell: "/bin/bash", stdio: "ignore" });
  } catch {}
}
function start() {
  killPort();
  return spawn(oj, ["dev", playground, "--port", String(PORT)], { stdio: "ignore" });
}
async function up() {
  await waitUp(`http://localhost:${PORT}/`);
}
const parsedModules = async () =>
  (await (await fetch(`http://localhost:${PORT}/__oj_parsed`)).text()).trim().split(",");
function fail(msg) {
  console.error("FAIL:", msg);
  process.exit(1);
}

fs.rmSync(path.join(playground, ".oj-cache"), { recursive: true, force: true });
let srv = start();
try {
  await up();
  await fetch(`http://localhost:${PORT}/`);
  await sleep(800);
  if (!(await parsedModules()).includes("App.tsx")) fail("cold: moduleParsed missing App.tsx");
} finally {
  srv.kill("SIGKILL");
}
await sleep(500);

srv = start();
try {
  await up();
  await fetch(`http://localhost:${PORT}/`);
  await sleep(800);

  if (!(await parsedModules()).includes("App.tsx")) {
    fail("warm: moduleParsed did not replay App.tsx (transform side effect lost on cache hit)");
  }
  console.log("warm-cache: moduleParsed replayed for cache-hit App.tsx");

  const original = fs.readFileSync(WATCHED, "utf8");
  const ws = new WebSocket(`ws://localhost:${PORT}/__ws`);
  const gotReload = new Promise((resolve) => {
    ws.addEventListener("message", (e) => {
      try {
        if (JSON.parse(e.data).type === "full-reload") resolve(true);
      } catch {}
    });
  });
  await new Promise((resolve, reject) => {
    ws.addEventListener("open", resolve);
    ws.addEventListener("error", reject);
  });
  fs.writeFileSync(WATCHED, "warm-v2\n");
  const reloaded = await Promise.race([gotReload, sleep(8000).then(() => false)]);
  fs.writeFileSync(WATCHED, original);
  ws.close();
  if (!reloaded) {
    fail("warm: addWatchFile watch lost on cache hit (no full-reload on plugin-watched.txt edit)");
  }
  console.log("warm-cache: addWatchFile re-applied -> full reload on watched-file edit");
} finally {
  srv.kill("SIGKILL");
  killPort();
}

console.log("\nWARM-CACHE REGRESSION PASSED");
