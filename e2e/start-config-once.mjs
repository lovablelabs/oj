// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Vite's createServer evaluates vite.config once and runs each plugin's
// config hook once. Start dev evaluates it in each plugin host (their plugin
// instances cannot cross engines) and in a cold config extraction, but never
// for instances nothing serves: every evaluation but the extraction's must
// be one whose plugins run. Checked on the first boot and after a config edit
// restarts the server. Run with a built target/debug/oj (or OJ_BIN).
import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { restartedBoot, settles, sleep, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 6871);

const installed = fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start"));
if (!installed) {
  console.log("SKIP start config once: fixture deps not installed");
  process.exit(0);
}

if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-config-once-"));
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

// Each evaluation logs an id; its plugin logs the id on every config hook and
// once when it first serves (configureServer, buildStart or resolveId).
const evals = path.join(tmp, "evals.log");
const configFile = path.join(app, "vite.config.ts");
const source = fs.readFileSync(configFile, "utf8").replace(
  "plugins: [",
  `plugins: [
    {
      name: "e2e-config-counter",
      config() {
        countFs.appendFileSync(${JSON.stringify(evals)}, \`config \${countId}\\n\`);
      },
      configureServer() {
        countServed();
      },
      buildStart() {
        countServed();
      },
      resolveId() {
        countServed();
      },
    },`,
);
const header = `import * as countFs from "node:fs";
const countId = Math.random().toString(36).slice(2);
countFs.appendFileSync(${JSON.stringify(evals)}, \`eval \${countId}\\n\`);
let countServedOnce = false;
function countServed() {
  if (countServedOnce) return;
  countServedOnce = true;
  countFs.appendFileSync(${JSON.stringify(evals)}, \`served \${countId}\\n\`);
}
`;
fs.writeFileSync(configFile, header + source);

const read = (from) => fs.readFileSync(evals, "utf8").slice(from).split("\n").filter(Boolean);
// One boot's evaluations from byte `from` on: the ids no plugin ever served
// from, and the ids whose config hook did not run exactly once.
function tally(from) {
  const lines = read(from).map((l) => l.split(" "));
  const ids = lines.filter(([k]) => k === "eval").map(([, id]) => id);
  const served = new Set(lines.filter(([k]) => k === "served").map(([, id]) => id));
  const configs = (id) => lines.filter(([k, i]) => k === "config" && i === id).length;
  return { ids, unserved: ids.filter((id) => !served.has(id)), reconfigured: ids.filter((id) => configs(id) !== 1) };
}
async function checkBoot(label, from) {
  await waitUp(`http://localhost:${PORT}/`, { proc: srv });
  const entry = await fetch(`http://localhost:${PORT}/@oj-start/client-entry.js`);
  if (entry.status !== 200) throw new Error(`${label}: client entry returned ${entry.status}`);
  let t = tally(from);
  await settles(() => (t = tally(from)).unserved.length <= 1, { timeoutMs: 20000 });
  if (t.unserved.length > 1) {
    throw new Error(
      `${label}: ${t.ids.length} evaluations, ${t.unserved.length} never served: ${t.unserved.join(", ")}`,
    );
  }
  if (t.reconfigured.length) throw new Error(`${label}: config hook not run once for ${t.reconfigured.join(", ")}`);
  console.log(`${label}: ${t.ids.length} evaluation(s), ${t.unserved.length} unserved`);
}

fs.writeFileSync(evals, "");
let log = "";
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
srv.stdout.on("data", (c) => (log += c));
srv.stderr.on("data", (c) => (log += c));

let failed = false;
try {
  await checkBoot("boot", 0);

  const from = fs.statSync(evals).size;
  const mark = log.length;
  fs.appendFileSync(configFile, "\n// edited\n");
  if (!(await settles(() => log.slice(mark).includes("restarting dev server"), { timeoutMs: 30000 }))) {
    throw new Error(`the config edit did not restart; log tail:\n${log.slice(-3000)}`);
  }
  await restartedBoot(() => log, mark);
  await checkBoot("restart", from);
  console.log("START-CONFIG-ONCE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("START-CONFIG-ONCE E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(tmp, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
}
process.exit(failed ? 1 : 0);
