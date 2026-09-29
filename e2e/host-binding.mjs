// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

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

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

function lanIp() {
  for (const list of Object.values(os.networkInterfaces())) {
    for (const ni of list || []) {
      if (ni.family === "IPv4" && !ni.internal) return ni.address;
    }
  }
  return null;
}

async function reachable(ip, port) {
  return waitUp(`http://${ip}:${port}/`, { init: () => ({ signal: AbortSignal.timeout(1500) }) }).then(
    () => true,
    () => false,
  );
}

async function probe(ip, port) {
  try {
    await fetch(`http://${ip}:${port}/`, { signal: AbortSignal.timeout(1500) });
    return "open";
  } catch (e) {
    const code = e?.cause?.code || e?.code || e?.name;
    if (code === "ECONNREFUSED") return "refused";
    return "filtered";
  }
}

const lan = lanIp();
if (!lan) {
  console.log("HOST-BINDING E2E SKIPPED (no non-loopback IPv4 interface)");
  process.exit(0);
}

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-host-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "host-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "main.js"), `window.__READY = true;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

function start(args) {
  return spawn(oj, args, { stdio: "ignore" });
}

let failed = false;
try {
  execSync(`${oj} build ${app}`, { stdio: "ignore" });

  {
    const srv = start(["preview", app, "--port", "5481"]);
    try {
      assert.ok(await reachable("127.0.0.1", 5481), "preview reachable on loopback");
      assert.notEqual(await probe(lan, 5481), "open", "default preview NOT bound on the LAN address");
    } finally {
      srv.kill("SIGKILL");
      await sleep(300);
    }
    console.log("[preview] default binds loopback only OK");
  }

  {
    const srv = start(["preview", app, "--port", "5482", "--host"]);
    try {
      assert.ok(await reachable(lan, 5482), "preview --host reachable on the LAN address");
    } finally {
      srv.kill("SIGKILL");
      await sleep(300);
    }
    console.log("[preview] --host binds all interfaces OK");
  }

  {
    const srv = start(["dev", app, "--port", "5483", "--host"]);
    try {
      assert.ok(await reachable(lan, 5483), "dev --host reachable on the LAN address");
    } finally {
      srv.kill("SIGKILL");
      await sleep(300);
    }
    console.log("[dev] --host binds all interfaces OK");
  }

  {
    fs.writeFileSync(path.join(app, "oj.config.js"), `export default { server: { host: "::" } };\n`);
    fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
    const srv = start(["dev", app, "--port", "5484"]);
    try {
      assert.ok(await reachable(lan, 5484), "dev with server.host ':: ' reachable on LAN (dual-stack normalized)");
    } finally {
      srv.kill("SIGKILL");
      await sleep(300);
    }
    fs.rmSync(path.join(app, "oj.config.js"), { force: true });
    console.log("[dev] config host '::' binds all interfaces OK");
  }

  console.log("HOST-BINDING E2E PASSED");
} catch (err) {
  failed = true;
  console.error("HOST-BINDING E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
