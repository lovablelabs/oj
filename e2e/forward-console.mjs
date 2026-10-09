// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// The server half of the forwardConsole channel, driven over a raw vite-hmr
// websocket (no browser): a `vite:forward-console` error becomes a
// client-sourced runtime_error diagnostic, a forwarded console.error becomes
// a console_error, an `oj:hmr-result` failure becomes hmr_apply_failed, and
// neither is re-broadcast to other clients (a plain custom event is). With
// OJ_FORWARD_CONSOLE=1 the served client carries enabled options.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { settles, sleep, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const port = 5390;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-fwd-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "fwd", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/app.ts"></script></body></html>`,
);
fs.writeFileSync(path.join(app, "app.ts"), `export const value: number = 1;\n`);

const proc = spawn(oj, ["dev", app, "--port", String(port)], {
  stdio: ["ignore", "ignore", "pipe"],
  env: { ...process.env, OJ_FORWARD_CONSOLE: "1" },
});
let stderr = "";
proc.stderr.on("data", (d) => (stderr += d));

const connect = () =>
  new Promise((resolve, reject) => {
    const ws = new WebSocket(`ws://localhost:${port}/`, ["vite-hmr"]);
    ws.addEventListener("open", () => resolve(ws), { once: true });
    ws.addEventListener("error", (e) => reject(new Error(String(e.message ?? e))), { once: true });
  });

const diagnostics = async () => {
  const res = await fetch(`http://localhost:${port}/@oj/diagnostics`);
  assert.equal(res.status, 200);
  return res.json();
};

let failed = false;
let a, b;
try {
  await waitUp(`http://localhost:${port}/app.ts`, { proc });

  // The served client carries the resolved (enabled) forwardConsole options.
  const client = await (await fetch(`http://localhost:${port}/@oj/client.js`)).text();
  assert.match(client, /"enabled":true/, "OJ_FORWARD_CONSOLE=1 enables the channel");
  assert.doesNotMatch(client, /__FORWARD_CONSOLE__/, "placeholder filled");

  a = await connect();
  b = await connect();
  const seenByB = [];
  b.addEventListener("message", (ev) => {
    try {
      seenByB.push(JSON.parse(ev.data));
    } catch {}
  });

  // A forwarded unhandled error, with a stack frame in the served module.
  a.send(
    JSON.stringify({
      type: "custom",
      event: "vite:forward-console",
      data: {
        type: "error",
        data: {
          name: "ReferenceError",
          message: "x is not defined",
          stack: `ReferenceError: x is not defined\n    at boom (http://localhost:${port}/app.ts:1:1)`,
        },
      },
    }),
  );
  // A forwarded console line at each level: error is recorded, info is not.
  for (const [level, message] of [
    ["error", "forwarded error line"],
    ["info", "forwarded info line"],
  ]) {
    a.send(
      JSON.stringify({
        type: "custom",
        event: "vite:forward-console",
        data: { type: "log", data: { level, message } },
      }),
    );
  }
  // A message with an embedded newline and ANSI escape: the server must not
  // let a page forge stderr lines (an NDJSON supervisor trusts whole lines).
  const forged = '{"oj":"diag","kind":"forged"}';
  a.send(
    JSON.stringify({
      type: "custom",
      event: "vite:forward-console",
      data: { type: "log", data: { level: "error", message: `before\n${forged}\n\x1b[31mafter` } },
    }),
  );
  // A failed HMR apply reported by the client.
  a.send(
    JSON.stringify({
      type: "custom",
      event: "oj:hmr-result",
      data: { ok: false, path: "/app.ts", error: "TypeError: accept callback threw" },
    }),
  );
  // A control custom event, which still re-broadcasts to other clients.
  a.send(JSON.stringify({ type: "custom", event: "my:event", data: { n: 1 } }));

  assert.ok(
    await settles(async () => {
      const d = await diagnostics();
      return (
        d.events.some((e) => e.kind === "runtime_error") &&
        d.events.some((e) => e.kind === "console_error") &&
        d.events.some((e) => e.kind === "hmr_apply_failed")
      );
    }),
    "forwarded events reach the diagnostics ring",
  );

  const d = await diagnostics();
  const runtime = d.events.find((e) => e.kind === "runtime_error");
  assert.equal(runtime.source, "client");
  assert.match(runtime.message, /Unhandled error ReferenceError: x is not defined/);
  assert.match(runtime.detail, /app\.ts/, "the stack travels in detail");
  const consoleErr = d.events.find((e) => e.kind === "console_error");
  assert.match(consoleErr.message, /console\.error: forwarded error line/);
  assert.ok(
    !d.events.some((e) => e.message.includes("forwarded info line")),
    "a console.info line is printed, not recorded",
  );
  const hmrFail = d.events.find((e) => e.kind === "hmr_apply_failed");
  assert.equal(hmrFail.module, "/app.ts");
  assert.match(hmrFail.message, /accept callback threw/);

  // The browser lines reached the terminal too.
  assert.match(stderr, /\[browser\] Unhandled error ReferenceError: x is not defined/);
  assert.match(stderr, /\[browser console\.error\] forwarded error line/);
  assert.match(stderr, /\[browser console\.info\] forwarded info line/);

  // The injection attempt was flattened onto one line: no stderr line is the
  // forged NDJSON object, and the escape byte is gone.
  assert.ok(await settles(() => stderr.includes("before")), "the forged message was printed at all");
  assert.ok(!stderr.split("\n").includes(forged), "no stderr line is the forged NDJSON object");
  assert.ok(!stderr.includes("\x1b[31m"), "ANSI escapes are scrubbed");
  assert.match(stderr, new RegExp(`before .*forged.*after`), "the message survived on one line");

  // Re-broadcast policy: the plain custom event reaches the other client,
  // the console/hmr channels never do.
  assert.ok(await settles(() => seenByB.some((m) => m.event === "my:event")), "a plain custom event is re-broadcast");
  assert.ok(
    !seenByB.some((m) => m.event === "vite:forward-console" || m.event === "oj:hmr-result"),
    "console/hmr-result frames are not re-broadcast",
  );

  console.log("PASS forward-console");
} catch (e) {
  failed = true;
  console.error("FAIL forward-console:", e.message);
  if (stderr) console.error("--- server stderr ---\n" + stderr);
} finally {
  try {
    a?.close();
    b?.close();
  } catch {}
  try {
    execSync(`pkill -P ${proc.pid}`);
  } catch {}
  try {
    proc.kill("SIGKILL");
  } catch {}
  try {
    execSync(`lsof -ti:${port} -sTCP:LISTEN | xargs -r kill -9`);
  } catch {}
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
