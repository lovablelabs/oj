// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Watcher parity with Vite:
//  - editing a file the config imports (configFileDependencies) restarts the
//    dev server, not just vite.config itself;
//  - a change under `server.watch.ignored` produces no HMR frame;
//  - a change inside a nested node_modules produces no HMR frame either.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const port = Number(process.env.OJ_E2E_PORT || 5492);

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-watch-"));
fs.mkdirSync(path.join(app, "src", "generated"), { recursive: true });
fs.mkdirSync(path.join(app, "packages", "lib", "node_modules", "dep"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "watch-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "config-helper.mjs"), `export const ignored = ["src/generated/**"];\n`);
fs.writeFileSync(
  path.join(app, "vite.config.mjs"),
  `import { ignored } from "./config-helper.mjs";\nexport default { server: { watch: { ignored } } };\n`,
);
fs.writeFileSync(path.join(app, "src", "generated", "out.js"), `export const g = 1;\n`);
fs.writeFileSync(path.join(app, "src", "main.js"), `import { g } from "./generated/out.js";\ndocument.title = "v" + g;\nif (import.meta.hot) import.meta.hot.accept();\n`);
fs.writeFileSync(path.join(app, "packages", "lib", "node_modules", "dep", "index.js"), `export default 1;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

async function up() {
  for (let i = 0; i < 100; i++) { try { if ((await fetch(`http://localhost:${port}/`)).ok) return true; } catch {} await sleep(200); }
  return false;
}

let failed = false;
const log = path.join(app, "server.log");
const fd = fs.openSync(log, "w");
const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: ["ignore", fd, fd] });
try {
  assert.ok(await up(), "server started");
  // No .oj-cache rm here, on purpose: the app dir is a fresh mkdtemp (nothing
  // stale to clear), the watcher never watches .oj-cache, and a recursive rm
  // against a LIVE server loses to its legitimate cache writers (engine code
  // caches, engine-job children rewriting extraction scripts, the optimizer)
  // no matter how many retries — the recurring ENOTEMPTY flake in this test.

  const frames = [];
  const ws = new WebSocket(`ws://localhost:${port}/__ws`);
  ws.addEventListener("message", (ev) => { try { frames.push(JSON.parse(ev.data)); } catch {} });
  await new Promise((resolve, reject) => {
    ws.addEventListener("open", resolve);
    ws.addEventListener("error", () => reject(new Error("socket errored")));
  });
  await (await fetch(`http://localhost:${port}/src/main.js`)).text();
  await sleep(300);

  // 1. an ignored path: no frame at all
  let n = frames.length;
  fs.writeFileSync(path.join(app, "src", "generated", "out.js"), `export const g = 2;\n`);
  await sleep(1500);
  assert.equal(frames.length, n, `server.watch.ignored change produced frames: ${JSON.stringify(frames.slice(n))}`);

  // 2. nested node_modules: no frame
  n = frames.length;
  fs.writeFileSync(path.join(app, "packages", "lib", "node_modules", "dep", "index.js"), `export default 2;\n`);
  await sleep(1500);
  assert.equal(frames.length, n, `nested node_modules change produced frames: ${JSON.stringify(frames.slice(n))}`);

  // 3. a normal edit still updates (the watcher is alive)
  n = frames.length;
  fs.writeFileSync(path.join(app, "src", "main.js"), `import { g } from "./generated/out.js";\ndocument.title = "w" + g;\nif (import.meta.hot) import.meta.hot.accept();\n`);
  for (let i = 0; i < 50 && frames.length === n; i++) await sleep(100);
  assert.ok(frames.length > n && frames[frames.length - 1].type === "update", "a source edit still hot updates");

  // 4. a config dependency edit restarts the server
  fs.writeFileSync(path.join(app, "config-helper.mjs"), `export const ignored = ["src/generated/**", "other/**"];\n`);
  for (let i = 0; i < 100; i++) {
    if (fs.readFileSync(log, "utf8").includes("restarting dev server")) break;
    await sleep(100);
  }
  assert.match(fs.readFileSync(log, "utf8"), /restarting dev server/, "editing a config dependency restarts");
  assert.ok(await up(), "server came back after the restart");
  console.log("WATCH-CONFIG-DEPS-IGNORED E2E PASSED");
} catch (err) {
  failed = true;
  console.error("WATCH-CONFIG-DEPS-IGNORED E2E FAILED:", err.message);
  try { console.error(fs.readFileSync(log, "utf8").split("\n").slice(-20).join("\n")); } catch {}
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
