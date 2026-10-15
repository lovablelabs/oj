// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// A plugin's server.restart() re-execs oj, and the next boot must look like a
// fresh launch: the env the process started with, not what its engines wrote
// into it mid-session. A leaked NODE_ENV / VITE_* / TSS_* re-keyed the Start
// client bundle cache, so every restart rebuilt the bundle from scratch, and
// a leaked .env value posed as shell env on the next boot, outranking an
// edited .env file. Run with a built target/debug/oj (or OJ_BIN).
import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { restartedBoot, settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = 6869;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const installed =
  fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(fixture, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP start plugin restart: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-plugin-restart-"));
const app = path.join(tmp, "app");
fs.mkdirSync(app);
for (const f of [
  "src",
  "public",
  "styles",
  "packages",
  "tsconfig.json",
  "package.json",
  "vite.config.ts",
  ".env.development",
]) {
  fs.cpSync(path.join(fixture, f), path.join(app, f), { recursive: true });
}
fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");

// A plugin that calls server.restart() when the trigger file is touched, the
// way @cloudflare/vite-plugin restarts after its runtime fails. Only a touch
// newer than the plugin's own boot counts, so the restarted boot does not
// restart again, and the Start script children (which run configureServer
// too, with no server to restart) never consume a trigger.
const trigger = path.join(tmp, "restart-now");
const configFile = path.join(app, "vite.config.ts");
const config = fs.readFileSync(configFile, "utf8").replace(
  "plugins: [",
  `plugins: [
    {
      name: "e2e-restarter",
      configureServer(server) {
        const booted = Date.now();
        const poll = setInterval(() => {
          let touched = 0;
          try {
            touched = restartFs.statSync(${JSON.stringify(trigger)}).mtimeMs;
          } catch {}
          if (touched <= booted) return;
          clearInterval(poll);
          server.restart();
        }, 200);
      },
    },`,
);
fs.writeFileSync(configFile, `import * as restartFs from "node:fs";\n${config}`);

let log = "";
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
srv.stdout.on("data", (c) => (log += c));
srv.stderr.on("data", (c) => (log += c));

const restarts = () => log.split("restarting dev server").length - 1;
const page = async () => (await fetch(`http://localhost:${PORT}/`)).text();

let failed = false;
try {
  await waitUp(`http://localhost:${PORT}/`, { proc: srv });
  if (!(await settles(() => log.includes("client bundle cached"), { timeoutMs: 30000 }))) {
    throw new Error(`the first boot never cached its client bundle; log tail:\n${log.slice(-3000)}`);
  }

  // The plugin restart: nothing the bundle depends on changed, so the next
  // boot restores the cached client bundle instead of rebuilding it.
  let mark = log.length;
  fs.writeFileSync(trigger, "");
  if (!(await settles(() => restarts() >= 1, { timeoutMs: 30000 }))) {
    throw new Error(`server.restart() did not restart; log tail:\n${log.slice(-3000)}`);
  }
  // The .env edit below needs the restarted process watching.
  await restartedBoot(() => log, mark);
  await waitUp(`http://localhost:${PORT}/`, { proc: srv });
  const afterRestart = () => log.slice(mark);
  if (!(await settles(() => /client bundle (restored|cache miss)/.test(afterRestart()), { timeoutMs: 30000 }))) {
    throw new Error(`the restarted boot never reached the bundle cache; log tail:\n${log.slice(-3000)}`);
  }
  const miss = afterRestart().match(/client bundle cache miss \(([^)]*)\)/);
  if (miss) throw new Error(`the plugin restart rebuilt the client bundle (cache miss: ${miss[1]})`);
  console.log("plugin restart: the restarted boot restored the cached client bundle");

  // A .env edit restarts too, and the edited value must win: a value the
  // first boot leaked into the process env would pose as shell env here.
  mark = log.length;
  const before = restarts();
  fs.writeFileSync(
    path.join(app, ".env.development"),
    "VITE_FIXTURE_FLAVOR=edited-flavor\nFIXTURE_EDITION=custom-prefix-edition\n",
  );
  if (!(await settles(() => restarts() > before, { timeoutMs: 30000 }))) {
    throw new Error(`the .env edit did not restart; log tail:\n${log.slice(-3000)}`);
  }
  await waitUp(`http://localhost:${PORT}/`, { proc: srv });
  let html = "";
  const edited = await settles(
    async () => {
      html = await page().catch(() => "");
      return html.includes("jsenv:development:true:edited-flavor");
    },
    { timeoutMs: 30000 },
  );
  if (!edited) {
    throw new Error(`the edited .env value lost to the leaked one: ${html.match(/jsenv:[^<]*/)?.[0]}`);
  }
  console.log("plugin restart: an edited .env value wins after restarts");
  console.log("START-PLUGIN-RESTART E2E PASSED");
} catch (err) {
  failed = true;
  console.error("START-PLUGIN-RESTART E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  for (let i = 0; ; i++) {
    try {
      fs.rmSync(tmp, { recursive: true, force: true });
      break;
    } catch (e) {
      if (i >= 20) break;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100);
    }
  }
}
process.exit(failed ? 1 : 0);
