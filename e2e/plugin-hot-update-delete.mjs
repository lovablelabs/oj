// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite dispatches hotUpdate / watchChange for a removed file with type "delete"
// (the watcher's unlink) and for a new file with type "create" (add); oj used to
// skip deleted files entirely and always say "update" for the rest. A throwing
// hotUpdate is logged and sent to the client as an error payload (hmr.ts), not
// swallowed.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const PORT = 6404;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-hotdel-"));
// Outside the app root so the watcher never sees the mark file itself.
const marks = fs.mkdtempSync(path.join(os.tmpdir(), "oj-hotdel-marks-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "hotdel", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "a.js"), `export const A = "a";\n`);
fs.writeFileSync(path.join(app, "src", "gone.js"), `export const G = "g";\n`);
fs.writeFileSync(path.join(app, "src", "main.js"), `import "./a.js";\nimport "./gone.js";\nwindow.__ok = 1;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `import fs from "node:fs";
const log = (line) => fs.appendFileSync(${JSON.stringify(marks)} + "/events", line + "\\n");
export default [{
  name: "watcher",
  watchChange(id, { event }) { log("watchChange:" + id.split("/").pop() + ":" + event); },
  hotUpdate(ctx) {
    log("hotUpdate:" + ctx.file.split("/").pop() + ":" + ctx.type);
    if (ctx.file.endsWith("boom.js")) throw new Error("boom hook");
  },
}];\n`,
);

const events = () => (fs.existsSync(path.join(marks, "events")) ? fs.readFileSync(path.join(marks, "events"), "utf8").trim().split("\n") : []);

let failed = false;
let stderr = "";
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "ignore", "pipe"] });
srv.stderr.on("data", (d) => (stderr += d.toString()));
try {
  await waitUp(`http://localhost:${PORT}/`);
  await (await fetch(`http://localhost:${PORT}/src/main.js`)).text();
  await (await fetch(`http://localhost:${PORT}/src/a.js`)).text();
  await (await fetch(`http://localhost:${PORT}/src/gone.js`)).text();
  await sleep(500);

  fs.writeFileSync(path.join(app, "src", "a.js"), `export const A = "a2";\n`);
  await settles(() => events().some((e) => e.startsWith("hotUpdate:a.js")));
  assert.ok(events().includes("hotUpdate:a.js:update"), `an edited file is an update:\n${events().join("\n")}`);
  assert.ok(events().includes("watchChange:a.js:update"), `watchChange sees update:\n${events().join("\n")}`);

  fs.rmSync(path.join(app, "src", "gone.js"));
  await settles(() => events().some((e) => e.startsWith("hotUpdate:gone.js")));
  assert.ok(events().includes("hotUpdate:gone.js:delete"), `a removed file reaches hotUpdate with type delete:\n${events().join("\n")}`);
  assert.ok(events().includes("watchChange:gone.js:delete"), `watchChange sees delete:\n${events().join("\n")}`);

  fs.writeFileSync(path.join(app, "src", "fresh.js"), `export const F = "f";\n`);
  await settles(() => events().some((e) => e.startsWith("hotUpdate:fresh.js")));
  assert.ok(events().includes("hotUpdate:fresh.js:create"), `a new file reaches hotUpdate with type create:\n${events().join("\n")}`);
  assert.ok(events().includes("watchChange:fresh.js:create"), `watchChange sees create:\n${events().join("\n")}`);

  // A throwing hotUpdate: logged on the server and delivered to the client as
  // an error payload (Vite's environment.hot.send({ type: "error" })).
  const ws = new WebSocket(`ws://localhost:${PORT}/__ws`);
  await new Promise((res, rej) => {
    ws.addEventListener("open", res);
    ws.addEventListener("error", () => rej(new Error("ws open failed")));
  });
  const errorFrame = new Promise((res, rej) => {
    const timer = setTimeout(() => rej(new Error("no error frame after the throwing hotUpdate")), 8000);
    ws.addEventListener("message", (e) => {
      let m;
      try { m = JSON.parse(e.data); } catch { return; }
      if (m.type === "error") { clearTimeout(timer); res(m); }
    });
  });
  fs.writeFileSync(path.join(app, "src", "boom.js"), `export const B = 1;\n`);
  const frame = await errorFrame;
  assert.match(frame.err.message, /\[plugin:watcher\] boom hook/, "the client error names the plugin");
  await settles(() => stderr.includes("hotUpdate failed"));
  assert.match(stderr, /hotUpdate failed for .*boom\.js: .*boom hook/, `the server logs the hook error:\n${stderr}`);
  ws.close();
  console.log("PLUGIN-HOT-UPDATE-DELETE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PLUGIN-HOT-UPDATE-DELETE E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
  fs.rmSync(marks, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
