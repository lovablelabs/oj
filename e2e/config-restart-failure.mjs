// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite's restartServer creates the new server before it replaces the running
// one: when that fails (a vite.config whose plugin throws while the config
// resolves, applyToEnvironment included), it logs why plus "server restart
// failed" and keeps serving. The same config fails a fresh `vite dev`, and
// oj's boot. Every restart is checked so (a config edit, a plugin's
// `server.restart()`), off the watcher: the rest of the edit's batch is still
// handled, and a config evaluation that never ends holds back only the
// restart. Standalone: the restart re-execs oj.

import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { restartedBoot, settles, sleep, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const OJ = process.env.OJ_BIN ?? path.join(here, "..", "target", "debug", "oj");
const vite = path.join(here, "fixtures", "start-app", "node_modules", "vite");
const PORT = Number(process.env.OJ_E2E_PORT || 5253);
const BASE = `http://localhost:${PORT}/`;

if (!fs.existsSync(vite)) {
  console.log("SKIP config-restart-failure: start-app fixture not installed");
  process.exit(0);
}

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-restart-failure-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.mkdirSync(path.join(app, "node_modules"), { recursive: true });
fs.symlinkSync(vite, path.join(app, "node_modules", "vite"), "dir");
fs.writeFileSync(path.join(app, "package.json"), '{"name":"restart-failure","private":true,"type":"module"}');
fs.writeFileSync(
  path.join(app, "index.html"),
  '<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>',
);
fs.writeFileSync(path.join(app, "src", "main.js"), 'document.body.dataset.ok = "1";\n');
const config = (refuse) => `export default {
  plugins: [{
    name: "refuses-ssr",
    applyToEnvironment(environment) {
      if (${refuse} && environment.name === "ssr") throw new Error("refuses-ssr-environment");
      return true;
    },
    transform(code) { return code; },
    watchChange(id) { console.log("watch-change " + id.split("/").pop()); },
    configureServer(server) {
      server.middlewares.use((req, res, next) => {
        if (req.url !== "/__restart") return next();
        server.restart();
        res.end("restarting");
      });
    },
  }],
};
`;
const main = (marker) =>
  fs.writeFileSync(path.join(app, "src", "main.js"), `document.body.dataset.ok = "${marker}";\n`);
// The watcher handled the app file: the plugin's watchChange saw it.
const watched = (from) => settles(() => log.slice(from).includes("watch-change main.js"), { timeoutMs: 20000 });
fs.writeFileSync(path.join(app, "vite.config.mjs"), config(false));

let log = "";
const child = spawn(OJ, ["dev", "--port", String(PORT)], { cwd: app, detached: true });
child.stderr.on("data", (d) => (log += d.toString()));
child.stdout.on("data", (d) => (log += d.toString()));

let failed = false;
try {
  await waitUp(BASE, { proc: child });
  await (await fetch(`${BASE}src/main.js`)).text();
  const from = log.length;
  // One batch: the broken config and an app file.
  fs.writeFileSync(path.join(app, "vite.config.mjs"), config(true));
  main("batched-edit");
  if (!(await settles(() => /server restart failed/.test(log.slice(from)), { timeoutMs: 30000 }))) {
    throw new Error(`no "server restart failed" after the broken edit:\n${log.slice(from)}`);
  }
  if (!log.slice(from).includes("refuses-ssr-environment"))
    throw new Error(`the cause is not logged:\n${log.slice(from)}`);
  if (/restarting dev server/.test(log.slice(from)))
    throw new Error(`restarted into a failing boot:\n${log.slice(from)}`);
  await sleep(1000);
  const still = await fetch(BASE);
  if (still.status !== 200 || child.exitCode != null) throw new Error("the running server did not keep serving");
  console.log("broken config edit: running server kept, restart failure logged");
  if (!(await watched(from))) throw new Error("the app file of the config's batch was never handled");
  console.log("the rest of the batch: handled");

  const asked = log.length;
  await (await fetch(`${BASE}__restart`)).text();
  if (!(await settles(() => /server restart failed/.test(log.slice(asked)), { timeoutMs: 30000 }))) {
    throw new Error(`server.restart() on the broken config was not checked:\n${log.slice(asked)}`);
  }
  await sleep(1000);
  if ((await fetch(BASE)).status !== 200 || child.exitCode != null)
    throw new Error("server.restart() took the server down");
  console.log("plugin server.restart(): checked, running server kept");

  const fixed = log.length;
  fs.writeFileSync(path.join(app, "vite.config.mjs"), config(false));
  if (!(await settles(() => /restarting dev server/.test(log.slice(fixed)), { timeoutMs: 30000 }))) {
    throw new Error(`the fixed config did not restart:\n${log.slice(fixed)}`);
  }
  // The old process answers until it re-execs; the edit below must reach the
  // restarted one, or that one boots on the hanging config.
  await restartedBoot(() => log, fixed, { banner: "ready in" });
  await waitUp(BASE, { proc: child });
  console.log("fixed config edit: restarted");

  // A config evaluation that never ends holds back only the restart.
  fs.writeFileSync(path.join(app, "vite.config.mjs"), "await new Promise(() => {});\nexport default {};\n");
  await sleep(2000);
  const hung = log.length;
  main("after-a-hanging-config");
  if (!(await watched(hung))) throw new Error(`a hanging config check froze the watcher:\n${log.slice(fixed)}`);
  console.log("hanging config: the watcher keeps going");
  console.log("CONFIG-RESTART-FAILURE E2E PASSED");
} catch (e) {
  failed = true;
  console.error("CONFIG-RESTART-FAILURE E2E FAILED:", e.message);
} finally {
  try {
    process.kill(-child.pid, "SIGKILL");
  } catch {}
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
}
process.exit(failed ? 1 : 0);
