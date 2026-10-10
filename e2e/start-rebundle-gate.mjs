// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// The Start rebundle gate: a change whose paths miss every served graph (the
// client bundle's input closure, the engine's and the workers' module
// graphs) must not rebundle the client or reload the browser -- Vite does
// nothing for a file its graphs never served. A change to a bundled module
// must still rebundle and serve fresh. Run with a built target/debug/oj (or
// OJ_BIN).
import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = 6861;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const installed =
  fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(fixture, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP start rebundle gate: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-gate-"));
const app = path.join(tmp, "app");
fs.mkdirSync(app);
for (const f of ["src", "public", "styles", "packages", "tsconfig.json", "package.json", "vite.config.ts"]) {
  fs.cpSync(path.join(fixture, f), path.join(app, f), { recursive: true });
}
fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");
// Present before boot, so its edit below is an update of a known file, not a
// create (creates rebundle by design: resolution can shift onto new files).
fs.writeFileSync(path.join(app, "README.md"), "# fixture\n");

let log = "";
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
srv.stdout.on("data", (c) => (log += c));
srv.stderr.on("data", (c) => (log += c));

const get = async (route) => {
  const res = await fetch(`http://localhost:${PORT}${route}`);
  return { status: res.status, body: await res.text() };
};

let failed = false;
try {
  await waitUp(`http://localhost:${PORT}/`, { proc: srv });
  const about = await get("/about");
  if (about.status !== 200 || !about.body.includes("about-page-marker")) {
    throw new Error(`/about did not render (${about.status})`);
  }

  // A change outside every served graph: no rebundle, no reload.
  const bundledBefore = log.split("client bundled").length;
  fs.appendFileSync(path.join(app, "README.md"), "\nan edit outside every graph\n");
  await settles(async () => log.includes("change outside every served graph"), { timeoutMs: 10000 });
  await sleep(1500);
  const bundledAfter = log.split("client bundled").length;
  if (bundledAfter !== bundledBefore) {
    throw new Error(
      `README edit rebundled the client (${bundledAfter - bundledBefore} run(s)); log tail:\n${log.slice(-3000)}`,
    );
  }
  console.log("gate: README edit skipped the rebundle and the reload");

  // A bundled module still rebundles and reloads. (The served document's
  // freshness is the engine's own path and not asserted here: the base
  // behavior predates the gate and is unchanged by it.)
  const aboutFile = path.join(app, "src", "routes", "about.tsx");
  fs.writeFileSync(aboutFile, fs.readFileSync(aboutFile, "utf8").replace("about-page-marker", "about-page-edited"));
  await settles(async () => log.split("client bundled").length > bundledAfter, { timeoutMs: 20000 });
  if (log.split("client bundled").length === bundledAfter) {
    throw new Error(`route edit did not rebundle; log tail:\n${log.slice(-3000)}`);
  }
  await settles(async () => log.includes("rebuilt, reloading"), { timeoutMs: 10000 });
  if (!log.includes("rebuilt, reloading")) {
    throw new Error(`route edit did not reload; log tail:\n${log.slice(-3000)}`);
  }
  console.log("gate: bundled-module edit rebundled and reloaded");
  console.log("START-REBUNDLE-GATE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("START-REBUNDLE-GATE E2E FAILED:", err.message);
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
