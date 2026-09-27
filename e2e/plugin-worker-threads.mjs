// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Build plugins spawn workers: terser, workbox/vite-plugin-pwa and jest-worker
// all go through node:worker_threads, which deno_runtime implements on web
// workers. The engine used to ship the deno_runtime default worker callback —
// a panic ("not implemented: web workers are not supported") that took the
// whole build down for every PWA-enabled project. The worker must also get a
// `location` at bootstrap or the worker_threads polyfill dies with
// "Invalid URL: 'null'" before user code runs. This drives a config plugin
// through a spawn + message round-trip + terminate during `oj build`.

import { execSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

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
fs.writeFileSync(
  path.join(app, "vite.config.mjs"),
  `export default {
  plugins: [
    {
      name: "worker-probe",
      async buildStart() {
        const { Worker } = await import("node:worker_threads");
        const worker = new Worker(new URL("./worker-probe.mjs", import.meta.url), { workerData: 21 });
        const answer = await new Promise((resolve, reject) => {
          worker.once("message", resolve);
          worker.once("error", reject);
        });
        await worker.terminate();
        if (answer !== 42) throw new Error("worker answered " + answer);
        console.error("WORKER_ROUNDTRIP_OK");

        const sab = new SharedArrayBuffer(4);
        const sabWorker = new Worker(new URL("./worker-sab.mjs", import.meta.url), { workerData: sab });
        await new Promise((resolve, reject) => {
          sabWorker.once("message", resolve);
          sabWorker.once("error", reject);
        });
        await sabWorker.terminate();
        const seen = new Int32Array(sab)[0];
        if (seen !== 42) throw new Error("SharedArrayBuffer write not visible to the parent: " + seen);
        console.error("WORKER_SAB_OK");
      },
    },
  ],
};
`,
);

try {
  const r = spawnSync(oj, ["build", app], { encoding: "utf8" });
  const log = (r.stdout ?? "") + (r.stderr ?? "");
  if (r.status !== 0 || !log.includes("WORKER_ROUNDTRIP_OK") || !log.includes("WORKER_SAB_OK")) {
    console.error(log.slice(-4000));
    throw new Error(
      `worker_threads round-trip in a build plugin failed: status ${r.status}` +
        (log.includes("not implemented: web workers") ? " (worker callback panicked)" : "") +
        (log.includes("Invalid URL: 'null'") ? " (worker bootstrap has no location)" : "") +
        (!log.includes("WORKER_SAB_OK") && log.includes("WORKER_ROUNDTRIP_OK")
          ? " (SharedArrayBuffer did not cross the thread: shared stores unwired?)"
          : ""),
    );
  }
} finally {
  cleanup();
}
console.log("plugin-worker-threads: ok");
