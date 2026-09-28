// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite's ssr-deps playground: externalized CJS dependencies reach the SSR
// runner through Node's own require, and every EXPORT SHAPE must interop:
// a primitive module.exports, Object.assign exports, defineProperty getters,
// exports forwarded from another file, and TS-transpiled __esModule shapes.
// This is the richest CJS-interop bug farm in Vite's suite; oj's runner is
// homegrown, so each shape is pinned here.

import { execSync, spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const PORT = 5497;

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-ssr-interop-"));
const cleanup = () => fs.rmSync(app, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
const w = (rel, s) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), s);
};

const dep = (name, files) => {
  w(`node_modules/${name}/package.json`, JSON.stringify({ name, version: "1.0.0", main: "index.js" }));
  for (const [rel, s] of Object.entries(files)) w(`node_modules/${name}/${rel}`, s);
};
dep("shape-primitive", { "index.js": "module.exports = 42;\n" });
dep("shape-assign", { "index.js": "module.exports = Object.assign({}, { named: 'assign-named' });\n" });
dep("shape-defineprop", {
  "index.js":
    "Object.defineProperty(exports, '__esModule', { value: true });\n" +
    "Object.defineProperty(exports, 'named', { enumerable: true, get: () => 'dp-named' });\n" +
    "exports.default = 'dp-default';\n",
});
dep("shape-forwarded", {
  "index.js": "module.exports = require('./impl.js');\n",
  "impl.js": "exports.named = 'fwd-named';\n",
});
dep("shape-ts", {
  "index.js":
    "'use strict';\nObject.defineProperty(exports, '__esModule', { value: true });\n" +
    "exports.default = 'ts-default';\nexports.named = 'ts-named';\n",
});

w("package.json", JSON.stringify({ name: "ssr-interop", version: "1.0.0", type: "module" }));
w("index.html", `<!doctype html><html><body><div id="root"></div><script type="module" src="/src/main.js"></script></body></html>`);
w("src/main.js", "export const ok = 1;\n");
w(
  "vite.config.mjs",
  `export default { ssr: { external: ["shape-primitive", "shape-assign", "shape-defineprop", "shape-forwarded", "shape-ts"] } };\n`,
);
w(
  "src/entry-server.js",
  [
    "// Each shape probed at runtime so one failure cannot mask the rest.",
    "async function probe(spec) {",
    "  try {",
    "    const ns = await import(spec);",
    "    return { default: ns.default, named: ns.named ?? null, keys: Object.keys(ns).sort() };",
    "  } catch (e) { return { error: String(e && e.message || e).slice(0, 80) }; }",
    "}",
    "export async function render() {",
    "  const out = {};",
    '  for (const spec of ["shape-primitive", "shape-assign", "shape-defineprop", "shape-forwarded", "shape-ts"]) {',
    "    out[spec] = await probe(spec);",
    "  }",
    "  return `INTEROP:${encodeURIComponent(JSON.stringify(out))}:END`;",
    "}",
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
try {
  let body = "";
  for (let i = 0; i < 120; i++) {
    try {
      const res = await fetch(`http://localhost:${PORT}/`);
      body = await res.text();
      if (res.status === 200 && body.includes("INTEROP:")) break;
    } catch {}
    if (srv.exitCode !== null) throw new Error(`oj exited early\n${log.slice(-3000)}`);
    await sleep(300);
  }
  const must = (cond, msg) => {
    if (!cond) throw new Error(`${msg}\nrendered: ${body.slice(0, 1500)}\n${log.slice(-1500)}`);
  };
  const payload = body.match(/INTEROP:([^:]+):END/)?.[1];
  must(payload, "no INTEROP payload in the render");
  const shapes = JSON.parse(decodeURIComponent(payload));
  // VITE'S contract, asserted strictly (this test is ALLOWED to fail until
  // the runner matches it): Vite's SSR runner requires() externals and
  // interop-wraps them at runtime, so named exports exist even for shapes
  // cjs-module-lexer cannot see, and __esModule modules unwrap `default` to
  // exports.default. oj currently loads externals with Node's native interop
  // (default = module.exports always, lexer-visible names only); every
  // assertion below that fails is that exact gap.
  must(shapes["shape-primitive"].default === 42, "primitive module.exports must be the default import");
  must(shapes["shape-forwarded"].named === "fwd-named", "module.exports = require(...) must re-export named");
  must(shapes["shape-defineprop"].named === "dp-named", "defineProperty getter named export missed");
  must(shapes["shape-ts"].named === "ts-named", "TS-transpiled named export missed");
  must(shapes["shape-assign"].named === "assign-named", "VITE GAP: Object.assign-reassigned exports must expose named at runtime (Vite requires+proxies externals; Node's lexer cannot see this shape)");
  must(shapes["shape-ts"].default === "ts-default", "VITE GAP: an __esModule external's default must unwrap to exports.default (Node keeps the exports object; Vite unwraps)");
  must(shapes["shape-defineprop"].default === "dp-default", "VITE GAP: __esModule default unwrapping (defineProperty shape)");
  console.log("ssr-external-interop: every CJS export shape interops like Vite");
} catch (e) {
  failed = true;
  console.error("SSR EXTERNAL INTEROP FAILED:", e.message ?? e);
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
