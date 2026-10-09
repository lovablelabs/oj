// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// The in-host dependency scan must not starve the plugin host (issue #332):
// with many modules resolving through a slow plugin chain, the scan's resolve
// callbacks used to hold one event-loop pass for the whole crawl, so queued
// hooks got no reply inside the watchdog window and the host was declared
// gone (SSR answered 500 until restart). With the fairness gate the host
// keeps answering hooks mid-scan. The plugin spins only for `options.scan`
// calls, so request-path resolution stays cheap and any probe slowness is
// starvation, not the fixture's own cost. OJ_PLUGIN_TIMEOUT=2 shrinks the
// watchdog window to 4s so the red case fires fast.

import { spawn } from "node:child_process";
import { execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const viteSrc = path.join(repo, "e2e/fixtures/start-app/node_modules/vite");
if (!fs.existsSync(viteSrc)) {
  console.log("SKIP scan-starvation: vite fixture not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const PORT = 5367;
const MODULES = 1200;
const SPIN_MS = 6;

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-scanstarve-"));
const p = (rel) => path.join(app, rel);
fs.mkdirSync(p("node_modules"));
fs.symlinkSync(viteSrc, p("node_modules/vite"));
fs.writeFileSync(p("package.json"), '{"name":"scanstarve","private":true,"type":"module"}');
fs.writeFileSync(
  p("index.html"),
  '<!doctype html><html><body><script type="module" src="/main.js"></script></body></html>',
);
fs.mkdirSync(p("src"));
let main = "";
for (let i = 0; i < MODULES; i++) {
  fs.writeFileSync(p(`src/m${i}.js`), `export const v${i} = ${i};\n`);
  main += `import "./src/m${i}.js";\n`;
}
fs.writeFileSync(p("main.js"), `${main}export const ok = 1;\n`);
fs.writeFileSync(
  p("oj.plugins.mjs"),
  `import { writeFileSync } from "node:fs";
let marked = false;
export default [{
  name: "test:slow-scan-resolver",
  resolveId(id, importer, options) {
    if (!options?.scan) return null;
    if (!marked) {
      marked = true;
      writeFileSync(new URL("./scan-started.txt", import.meta.url), "1");
    }
    const until = Date.now() + ${SPIN_MS};
    while (Date.now() < until) {}
    return null;
  },
  transformIndexHtml(html) {
    return html;
  },
}];\n`,
);

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

let failed = false;
let stderr = "";
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], {
  stdio: ["ignore", "ignore", "pipe"],
  env: { ...process.env, OJ_PLUGIN_TIMEOUT: "2" },
});
srv.stderr.on("data", (d) => (stderr += d));
try {
  await waitUp(`http://localhost:${PORT}/`, { proc: srv });

  // The scan starts after boot (the optimizer task is spawned, not awaited);
  // the marker file is the plugin's first scan-path resolve.
  const scanning = await settles(() => fs.existsSync(p("scan-started.txt")), { timeoutMs: 60000 });
  must(scanning, `the dependency scan never reached the plugin\n${stderr.slice(-2000)}`);

  // Probe the host-hook path (transformIndexHtml) repeatedly while the scan
  // runs. Starvation shows up as probes stalling past the 4s watchdog window;
  // fairness keeps each one fast.
  let slowest = 0;
  const begun = Date.now();
  while (Date.now() - begun < (MODULES * SPIN_MS) / 2) {
    const t0 = Date.now();
    const res = await fetch(`http://localhost:${PORT}/`);
    const body = await res.text();
    must(
      res.ok,
      `"/" answered ${res.status} mid-scan after ${Date.now() - t0}ms: ${body.slice(0, 300)}\n${stderr.slice(-2000)}`,
    );
    slowest = Math.max(slowest, Date.now() - t0);
    await new Promise((r) => setTimeout(r, 250));
  }

  must(
    !/plugin host unresponsive/.test(stderr),
    `the scan starved the host into the watchdog:\n${stderr.slice(-2000)}`,
  );
  must(slowest < 2000, `a request stalled ${slowest}ms behind the scan (fairness gate not yielding)`);
  console.log(`probes stayed fast mid-scan (slowest ${slowest}ms), watchdog quiet`);
  console.log("\nSCAN STARVATION VERIFIED");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  try {
    srv.kill("SIGKILL");
  } catch {}
  fs.rmSync(app, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
}
process.exit(failed ? 1 : 0);
