// SPDX-License-Identifier: MIT
// Repeated HMR client rebundles must not grow the dev server. Each rebundle
// runs in a one-shot `oj start-script` child whose exit returns the bundler's
// native memory to the OS: rolldown's binding retains native memory per
// build() invocation, and nothing short of process exit releases it, so an
// in-process rebundle engine retained tens of MB PER EDIT on this small
// fixture (hundreds of MB on large apps), never releasing any of it. This
// test edits a route N times and asserts the dev server's RSS growth per
// rebundle stays bounded once the caches and lazy paths are warm.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "..");
const app = path.join(repo, "e2e", "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 5239);
const EDITS = 6;

const installed =
  fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(app, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP start-memory: fixture deps not installed");
  process.exit(0);
}
execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const must = (cond, msg) => { if (!cond) throw new Error(msg); };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const rm = (p) => {
  for (let i = 0; ; i++) {
    try {
      return fs.rmSync(p, { recursive: true, force: true });
    } catch (e) {
      if (i >= 20) throw e;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100);
    }
  }
};
const waitUp = async () => {
  for (let i = 0; i < 240; i++) {
    try { if ((await fetch(`http://localhost:${PORT}/`)).ok) return; } catch {}
    await sleep(500);
  }
  throw new Error(`server on :${PORT} did not start`);
};

const aboutFile = path.join(app, "src", "routes", "about.tsx");
const original = fs.readFileSync(aboutFile, "utf8");

rm(path.join(app, ".oj-cache"));
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], {
  stdio: ["ignore", "pipe", "pipe"],
});
let log = "";
srv.stdout.on("data", (d) => (log += d));
srv.stderr.on("data", (d) => (log += d));
const rebuilds = () => (log.match(/oj start: rebuilt, /g) || []).length;
const rssMb = () =>
  Number(execSync(`ps -o rss= -p ${srv.pid}`).toString().trim()) / 1024;

try {
  await waitUp();
  // Warm the SSR runner too, so later samples measure rebundles, not the
  // first render's imports.
  await fetch(`http://localhost:${PORT}/`);
  await sleep(1000);

  const samples = [];
  for (let i = 0; i < EDITS; i++) {
    const before = rebuilds();
    fs.writeFileSync(aboutFile, original + `\n// mem probe ${i} ${Date.now()}\n`);
    const deadline = Date.now() + 120000;
    while (Date.now() < deadline && rebuilds() <= before) await sleep(200);
    must(rebuilds() > before, `edit ${i}: no rebuild within 120s:\n${log.slice(-1500)}`);
    // Let the reload signal and any post-rebundle work settle before sampling.
    await sleep(750);
    samples.push(rssMb());
    console.log(`edit ${i}: rebuilds=${rebuilds()} rss=${samples[i].toFixed(0)}MB`);
  }

  const deltas = samples.slice(1).map((v, i) => v - samples[i]);
  console.log(`per-edit deltas: ${deltas.map((d) => d.toFixed(1)).join(", ")} MB`);
  // The first edits may still page in lazy machinery; judge the steady state.
  // The in-process rebundle engine retained ~30-40MB per edit on this fixture
  // (and kept growing forever); the one-shot child keeps the tail near zero.
  // Generous bounds for CI noise.
  const tail = deltas.slice(-3);
  const avgTail = tail.reduce((a, b) => a + b, 0) / tail.length;
  must(
    avgTail < 15,
    `dev server retains memory per rebundle: last-3-edit average ${avgTail.toFixed(1)}MB/edit (samples ${samples.map((s) => s.toFixed(0)).join(", ")} MB)`
  );
  must(
    samples[samples.length - 1] - samples[0] < 100,
    `dev server grew ${(samples[samples.length - 1] - samples[0]).toFixed(0)}MB over ${EDITS - 1} rebundles`
  );
  console.log("START MEMORY E2E PASSED");
} finally {
  fs.writeFileSync(aboutFile, original);
  srv.kill("SIGKILL");
  await sleep(300);
}
