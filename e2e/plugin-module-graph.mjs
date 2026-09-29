// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Plugins reach into `server.moduleGraph` from configureServer/hotUpdate:
// `getModuleById(id)` + `invalidateModule(mod)` must drop oj's compiled output
// for that module (so the next request re-runs plugin transforms) and propagate
// an HMR update for a real file; `server.restart()` must restart the server.
// Both used to be no-ops on a stub.

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
const PORT = 5503;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oj-modgraph-")));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "modgraph", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "a.js"), `export const A = "a";\n`);
fs.writeFileSync(path.join(app, "src", "main.js"), `import "./a.js";\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `let n = 0;
export default [{
  name: "graph-user",
  transform(code, id) {
    if (id.split("?")[0].endsWith("/src/a.js")) return code + "\\nglobalThis.__N = " + (++n) + ";\\n";
    return null;
  },
  configureServer(server) {
    server.middlewares.use("/__inv", (req, res) => {
      const mod = server.moduleGraph.getModuleById(${JSON.stringify(path.join(app, "src", "a.js"))});
      server.moduleGraph.invalidateModule(mod);
      res.end(mod ? "invalidated" : "no-module");
    });
    server.middlewares.use("/__restart", (req, res) => { res.end("restarting"); server.restart(); });
  },
}];\n`,
);

let failed = false;
let srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
const up = () => waitUp(`http://localhost:${PORT}/`);
try {
  await up();
  const first = await (await fetch(`http://localhost:${PORT}/src/a.js`)).text();
  assert.match(first, /__N = 1;/, `first transform:\n${first}`);
  const again = await (await fetch(`http://localhost:${PORT}/src/a.js`)).text();
  assert.match(again, /__N = 1;/, "an unchanged module is served from cache");

  const frames = [];
  const ws = new WebSocket(`ws://localhost:${PORT}/__ws`);
  await new Promise((r, j) => {
    ws.onopen = r;
    ws.onerror = j;
  });
  ws.onmessage = (m) => frames.push(String(m.data));

  assert.equal(
    await (await fetch(`http://localhost:${PORT}/__inv`)).text(),
    "invalidated",
    "getModuleById returned a node",
  );
  await sleep(500);
  const after = await (await fetch(`http://localhost:${PORT}/src/a.js`)).text();
  assert.match(after, /__N = 2;/, `invalidateModule must drop the cached compile so the transform re-runs:\n${after}`);
  assert.ok(
    frames.some((f) => /"type":"(update|full-reload)"/.test(f)),
    `invalidating a real file propagates an HMR message, got: ${frames.join(" | ")}`,
  );
  ws.close();

  // server.restart(): the process re-execs; the port comes back with fresh state.
  assert.equal(await (await fetch(`http://localhost:${PORT}/__restart`)).text(), "restarting");
  await sleep(1500);
  await up();
  const fresh = await (await fetch(`http://localhost:${PORT}/src/a.js`)).text();
  assert.match(fresh, /__N = 1;/, `after restart the plugin host is fresh (counter reset):\n${fresh}`);
  console.log("PLUGIN-MODULE-GRAPH E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PLUGIN-MODULE-GRAPH E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  try {
    execSync(`lsof -ti:${PORT} -sTCP:LISTEN | xargs kill -9`, { shell: "/bin/bash", stdio: "ignore" });
  } catch {}
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
