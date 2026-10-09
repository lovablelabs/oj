// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// The full forwardConsole loop through a real browser: a page runtime error
// and a console.error forwarded by the injected client land in
// /@oj/diagnostics as client-sourced events, with the error's stack remapped
// through the served module's source map (the event's module names the file).

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { settles, sleep, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
const port = 5393;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-fwdbrowser-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "fwdb", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/main.ts"></script></body></html>`,
);
// TypeScript so the served output differs from the source and the stack must
// be remapped; the throw escapes to window.onerror after module evaluation.
fs.writeFileSync(
  path.join(app, "main.ts"),
  `type Tag = string;
const tag: Tag = "boom from the page";
console.error("console says %s", tag);
// A hostile-for-formatting argument: a Symbol under %d. The wrapped console
// must not throw where the native one does not (\`__ready\` below is only
// reached if it didn't) and the Symbol formats as NaN, like Vite.
console.error("fmt %d %i %f ok", Symbol("s"), 42, 2.5);
setTimeout(() => {
  throw new Error(tag);
}, 0);
window.__ready = true;
`,
);

const proc = spawn(oj, ["dev", app, "--port", String(port)], {
  stdio: ["ignore", "ignore", "pipe"],
  env: { ...process.env, OJ_FORWARD_CONSOLE: "1" },
});
let stderr = "";
proc.stderr.on("data", (d) => (stderr += d));

let failed = false;
let browser;
try {
  await waitUp(`http://localhost:${port}/`, { proc });
  browser = await chromium.launch();
  const page = await browser.newPage();
  await page.goto(`http://localhost:${port}/`, { timeout: 30000 });
  await page.waitForFunction(() => window.__ready === true, { timeout: 10000 });

  const diagnostics = async () => {
    const res = await fetch(`http://localhost:${port}/@oj/diagnostics`);
    return res.json();
  };
  assert.ok(
    await settles(async () => {
      const d = await diagnostics();
      return d.events.some((e) => e.kind === "runtime_error") && d.events.some((e) => e.kind === "console_error");
    }),
    `the page's error and console.error reach the ring; stderr:\n${stderr}`,
  );

  const d = await diagnostics();
  const runtime = d.events.find((e) => e.kind === "runtime_error");
  assert.equal(runtime.source, "client");
  assert.match(runtime.message, /boom from the page/);
  // The remap proves itself through `module`: it is only set when a stack
  // frame mapped back through the served module's source map.
  assert.ok(
    runtime.module && runtime.module.endsWith("main.ts"),
    `the stack remapped to the source file, got module=${runtime.module} detail=${runtime.detail}`,
  );
  const consoleErrs = d.events.filter((e) => e.kind === "console_error");
  assert.ok(
    consoleErrs.some((e) => /console says boom from the page/.test(e.message)),
    "%s formatting applied",
  );
  assert.ok(
    consoleErrs.some((e) => /fmt NaN 42 2\.5 ok/.test(e.message)),
    `a Symbol under %d formats as NaN, floats keep their fraction; got ${JSON.stringify(consoleErrs.map((e) => e.message))}`,
  );

  console.log("PASS forward-console-browser");
} catch (e) {
  failed = true;
  console.error("FAIL forward-console-browser:", e.message);
  if (stderr) console.error("--- server stderr ---\n" + stderr);
} finally {
  try {
    await browser?.close();
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
