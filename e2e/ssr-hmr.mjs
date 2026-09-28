// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// SSR HMR, Vite's hmr-ssr contract reduced to its load-bearing core: the SSR
// runner keeps a module cache across requests; editing one module re-executes
// THAT module (the next render sees the change, with no server restart) while
// an untouched module's instance survives (its top-level ran once). Vite pins
// this across ~50 hmr-ssr cases; these are the two invariants everything
// there rests on.

import { execSync, spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const PORT = 5503;

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-ssr-hmr-"));
const cleanup = () => fs.rmSync(app, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
const w = (rel, s) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), s);
};
w("package.json", JSON.stringify({ name: "ssr-hmr", version: "1.0.0", type: "module" }));
w("index.html", `<!doctype html><html><body><div id="root"></div><script type="module" src="/src/main.js"></script></body></html>`);
w("src/main.js", "export const ok = 1;\n");
w("src/dep.js", 'export const msg = "m1";\n');
// Top-level execution counter: proves whether the runner re-executes this
// UNTOUCHED module when a sibling is edited.
w("src/state.js", "globalThis.__ssrExecs = (globalThis.__ssrExecs ?? 0) + 1;\nexport const execs = () => globalThis.__ssrExecs;\n");
w(
  "src/entry-server.js",
  [
    'import { msg } from "./dep.js";',
    'import { execs } from "./state.js";',
    "export async function render() { return `msg:${msg}|execs:${execs()}`; }",
    "export function load() { return Promise.resolve({}); }",
    "export function head() { return ''; }",
    "",
  ].join("\n"),
);

let failed = false;
const srv = spawn(oj, ["dev", app, "--ssr", "src/entry-server.js", "--port", String(PORT)], {
  stdio: ["ignore", "ignore", "pipe"],
  detached: true,
});
let log = "";
srv.stderr.on("data", (d) => (log += d));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const get = async () => (await fetch(`http://localhost:${PORT}/`)).text();
const field = (body, name) => body.match(new RegExp(`${name}:([^|<"]+)`))?.[1];
try {
  let body = "";
  for (let i = 0; i < 120; i++) {
    try {
      body = await get();
      if (body.includes("msg:")) break;
    } catch {}
    if (srv.exitCode !== null) throw new Error(`oj exited early\n${log.slice(-3000)}`);
    await sleep(300);
  }
  if (field(body, "msg") !== "m1") throw new Error(`first render wrong: ${body.slice(0, 300)}`);
  const execs0 = Number(field(body, "execs"));

  // Invariant 1: the runner caches modules across requests (repeated renders
  // do not re-execute state.js).
  const again = await get();
  const execs1 = Number(field(again, "execs"));
  if (execs1 !== execs0) {
    throw new Error(`the SSR runner re-executes modules per request: execs ${execs0} -> ${execs1}`);
  }

  // Invariant 2: editing dep.js reaches the next render without a server
  // restart, and the UNTOUCHED state.js instance survives the update.
  w("src/dep.js", 'export const msg = "m2";\n');
  let after = "";
  for (let i = 0; i < 80; i++) {
    after = await get();
    if (field(after, "msg") === "m2") break;
    await sleep(250);
  }
  if (field(after, "msg") !== "m2") throw new Error(`the dep edit never reached the SSR render:\n${after.slice(0, 300)}\n${log.slice(-2000)}`);
  if (srv.exitCode !== null) throw new Error("the server restarted (or died) for a source edit");
  const execs2 = Number(field(after, "execs"));
  if (execs2 !== execs0) {
    throw new Error(
      `editing dep.js re-executed the untouched state.js (execs ${execs0} -> ${execs2}): invalidation is not selective`,
    );
  }
  console.log(`ssr-hmr: module cache held across requests (execs=${execs0}), dep edit re-rendered selectively`);
} catch (e) {
  failed = true;
  console.error("SSR HMR FAILED:", e.message ?? e);
} finally {
  try {
    process.kill(-srv.pid, "SIGKILL");
  } catch {
    try {
      srv.kill("SIGKILL");
    } catch {}
  }
  cleanup();
}
process.exit(failed ? 1 : 0);
