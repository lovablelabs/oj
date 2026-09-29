// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// `oj dev` eagerly crawls the module graph by default (parallel pre-compile,
// warm HMR); `--lazy` opts into compiling on demand (Vite's model), which suits
// apps that code-split. This asserts both: the default logs "eager graph ready",
// and --lazy serves the same modules without ever running the crawl.

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
const port = 5318;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-lazy-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "lazy", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/main.tsx"></script></body></html>`,
);
fs.writeFileSync(path.join(app, "dep.tsx"), `export const dep = 7;\n`);
fs.writeFileSync(path.join(app, "unused.tsx"), `export const unused = 99;\n`);
fs.writeFileSync(
  path.join(app, "main.tsx"),
  `import { dep } from "./dep";\nexport const value = dep + 1;\nconsole.log(value);\n`,
);

// Run oj, collect stdout, request the entry + main, and report whether the eager
// crawl ran (it prints "eager graph ready").
async function run({ lazy }) {
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  const args = ["dev", app, "--port", String(port)];
  if (lazy) args.push("--lazy");
  const proc = spawn(oj, args, { stdio: ["ignore", "pipe", "ignore"] });
  let out = "";
  proc.stdout.on("data", (d) => (out += d.toString()));
  try {
    await waitUp(`http://localhost:${port}/`);
    const root = await fetch(`http://localhost:${port}/`);
    const main = await fetch(`http://localhost:${port}/main.tsx`);
    assert.equal(root.status, 200, "index serves");
    assert.equal(main.status, 200, "on-demand module serves");
    assert.match(await main.text(), /const value/, "module compiled");
    // Give the eager crawl (if any) time to finish and log.
    await settles(() => /eager graph ready/.test(out), { timeoutMs: 3000, pollMs: 150 });
    return out;
  } finally {
    try {
      execSync(`pkill -P ${proc.pid}`);
    } catch {}
    try {
      proc.kill("SIGKILL");
    } catch {}
    try {
      execSync(`lsof -ti:${port} -sTCP:LISTEN | xargs -r kill -9`);
    } catch {}
    await sleep(500);
  }
}

let failed = false;
try {
  const defaultOut = await run({ lazy: false });
  assert.match(defaultOut, /eager graph ready/, "default must run the eager crawl");

  const lazyOut = await run({ lazy: true });
  assert.ok(!/eager graph ready/.test(lazyOut), `--lazy must NOT run the eager crawl:\n${lazyOut}`);

  console.log("PASS lazy-flag");
} catch (e) {
  failed = true;
  console.error("FAIL lazy-flag:", e.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
