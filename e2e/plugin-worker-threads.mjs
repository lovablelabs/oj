// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Build plugins spawn workers: terser, workbox/vite-plugin-pwa and jest-worker
// all go through node:worker_threads, which deno_runtime implements on web
// workers. The engine used to ship the deno_runtime default worker callback,
// a panic ("not implemented: web workers are not supported") that took the
// whole build down for every PWA-enabled project. The worker must also get a
// `location` at bootstrap or it dies with "Invalid URL: 'null'" before user
// code runs. This drives a config plugin through: a spawn + message
// round-trip + terminate, a SharedArrayBuffer Atomics write, a worker
// spawning a worker (the callback is recursive), and a resourceLimits-capped
// worker that must die with ERR_WORKER_OUT_OF_MEMORY instead of aborting the
// whole build process, all during `oj build`.

import { execSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");

if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-worker-threads-"));
const cleanup = () => fs.rmSync(app, { recursive: true, force: true });
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(
  path.join(app, "package.json"),
  JSON.stringify({ name: "worker-threads-app", version: "1.0.0", type: "module" }),
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><body><div id="root"></div><script type="module" src="/src/main.js"></script></body></html>`,
);
fs.writeFileSync(path.join(app, "src", "main.js"), `document.getElementById("root").textContent = "ok";\n`);
fs.writeFileSync(
  path.join(app, "worker-probe.mjs"),
  `import { parentPort, workerData } from "node:worker_threads";\nparentPort.postMessage(workerData * 2);\n`,
);
// SharedArrayBuffer across threads: Node supports it natively; the engine
// needs the shared store pair wired between the parent and every worker
// (piscina's Atomics-based sync mode is the production shape).
fs.writeFileSync(
  path.join(app, "worker-sab.mjs"),
  `import { parentPort, workerData } from "node:worker_threads";\nconst view = new Int32Array(workerData);\nAtomics.store(view, 0, 42);\nparentPort.postMessage("stored");\n`,
);
// Workers spawn workers (jest-worker under a pooled plugin does): the
// engine's worker callback must be recursive.
fs.writeFileSync(
  path.join(app, "worker-nested.mjs"),
  `import { Worker, parentPort, workerData } from "node:worker_threads";
const child = new Worker(new URL("./worker-probe.mjs", import.meta.url), { workerData });
child.once("message", async (v) => {
  await child.terminate();
  parentPort.postMessage(v);
});
`,
);
// A runaway allocation under resourceLimits must kill THIS worker with
// ERR_WORKER_OUT_OF_MEMORY (Node semantics), not abort the build in a V8
// fatal OOM.
fs.writeFileSync(
  path.join(app, "worker-oom.mjs"),
  `const hog = [];\nfor (;;) hog.push(new Array(1024 * 1024).fill(Math.random()));\n`,
);
const settle = (worker) =>
  new Promise((resolve, reject) => {
    worker.once("message", resolve);
    worker.once("error", reject);
    worker.once("exit", (code) => reject(new Error("worker exited before posting, code " + code)));
  });
fs.writeFileSync(
  path.join(app, "vite.config.mjs"),
  `const settle = ${settle.toString()};
export default {
  plugins: [
    {
      name: "worker-probe",
      async buildStart() {
        const { Worker } = await import("node:worker_threads");
        const worker = new Worker(new URL("./worker-probe.mjs", import.meta.url), { workerData: 21 });
        const answer = await settle(worker);
        await worker.terminate();
        if (answer !== 42) throw new Error("worker answered " + answer);
        console.error("WORKER_ROUNDTRIP_OK");

        const sab = new SharedArrayBuffer(4);
        const sabWorker = new Worker(new URL("./worker-sab.mjs", import.meta.url), { workerData: sab });
        await settle(sabWorker);
        await sabWorker.terminate();
        const seen = new Int32Array(sab)[0];
        if (seen !== 42) throw new Error("SharedArrayBuffer write not visible to the parent: " + seen);
        console.error("WORKER_SAB_OK");

        const nested = new Worker(new URL("./worker-nested.mjs", import.meta.url), { workerData: 5 });
        const relayed = await settle(nested);
        await nested.terminate();
        if (relayed !== 10) throw new Error("nested worker relayed " + relayed);
        console.error("WORKER_NESTED_OK");

        const capped = new Worker(new URL("./worker-oom.mjs", import.meta.url), {
          resourceLimits: { maxOldGenerationSizeMb: 32 },
        });
        const oomErr = await new Promise((resolve) => {
          capped.once("error", resolve);
          capped.once("exit", (code) => resolve(new Error("exited " + code + " without an error event")));
        });
        await capped.terminate();
        const text = String((oomErr && oomErr.code) || "") + " " + String(oomErr);
        if (!text.includes("ERR_WORKER_OUT_OF_MEMORY") && !text.includes("memory limit")) {
          throw new Error("capped worker did not report its memory limit: " + text);
        }
        console.error("WORKER_OOM_LIMIT_OK");
      },
    },
  ],
};
`,
);

try {
  const r = spawnSync(oj, ["build", app], { encoding: "utf8", timeout: 300_000 });
  const log = (r.stdout ?? "") + (r.stderr ?? "");
  const marks = ["WORKER_ROUNDTRIP_OK", "WORKER_SAB_OK", "WORKER_NESTED_OK", "WORKER_OOM_LIMIT_OK"];
  const missing = marks.filter((m) => !log.includes(m));
  if (r.status !== 0 || missing.length > 0) {
    console.error(log.slice(-4000));
    throw new Error(
      `worker_threads in a build plugin failed: status ${r.status}, missing ${missing.join(",") || "none"}` +
        (log.includes("not implemented: web workers") ? " (worker callback panicked)" : "") +
        (log.includes("Invalid URL: 'null'") ? " (worker bootstrap has no location)" : "") +
        (missing.includes("WORKER_SAB_OK") && log.includes("WORKER_ROUNDTRIP_OK")
          ? " (SharedArrayBuffer did not cross the thread: shared stores unwired?)"
          : "") +
        (missing.includes("WORKER_OOM_LIMIT_OK") && r.status !== 0 && !log.includes("WORKER_OOM_LIMIT_OK")
          ? " (a capped worker may have taken the process down: fatal V8 OOM instead of ERR_WORKER_OUT_OF_MEMORY?)"
          : ""),
    );
  }
} finally {
  cleanup();
}
console.log("plugin-worker-threads: ok");
