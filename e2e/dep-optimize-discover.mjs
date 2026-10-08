// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Runtime dep discovery (Vite's registerMissingImport): a module only
// reachable through a runtime-built import() is invisible to the scan. The
// first serve rewrites its bare dep to the FUTURE optimized URL, the dep
// route holds that URL's requests until the debounced re-optimization
// commits, a stale ?v= is a 504 (Vite's outdated request), and the discovery
// persists in the manifest.

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
const viteSrc = path.join(repo, "e2e/fixtures/start-app/node_modules/vite");
const port = 5276;

if (!fs.existsSync(viteSrc)) {
  console.log("SKIP dep-optimize-discover: vite fixture not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-optdep-disc-"));
const nm = path.join(app, "node_modules");
fs.mkdirSync(path.join(nm, "cjs-lib"), { recursive: true });
fs.symlinkSync(viteSrc, path.join(nm, "vite"));
fs.writeFileSync(
  path.join(nm, "cjs-lib", "package.json"),
  JSON.stringify({ name: "cjs-lib", version: "1.0.0", main: "index.js" }),
);
fs.writeFileSync(path.join(nm, "cjs-lib", "index.js"), `exports.greet = function (n) { return "hi " + n; };\n`);
// The tanstack-start shape: an optimizeDeps.exclude'd package serves per-file,
// and its own bare import of a sibling must NOT register (Vite's resolve.ts
// skips importers inside node_modules), or the sibling gets a phantom bundle.
for (const [name, code] of [
  ["wrapper-lib", `import { inner } from "inner-lib";\nexport const viaWrapper = inner + "!";\n`],
  ["inner-lib", `export const inner = "from-inner";\n`],
]) {
  fs.mkdirSync(path.join(nm, name), { recursive: true });
  fs.writeFileSync(
    path.join(nm, name, "package.json"),
    JSON.stringify({ name, version: "1.0.0", type: "module", main: "index.js" }),
  );
  fs.writeFileSync(path.join(nm, name, "index.js"), code);
}
fs.writeFileSync(path.join(app, "oj.config.json"), JSON.stringify({ optimizeDeps: { exclude: ["wrapper-lib"] } }));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "optdep-disc-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/main.js"></script></body></html>`,
);
// The widget is reached only through a runtime-built specifier no static
// analysis can expand (rolldown's scan follows inline concat and template
// imports like dynamic-import-vars, so the parts must come from a variable):
// cjs-lib is not in the initial pre-bundle.
fs.writeFileSync(
  path.join(app, "main.js"),
  `const parts = ["", "widgets", "widget.js"];\nimport(parts.join("/")).then((m) => m.run());\n`,
);
fs.mkdirSync(path.join(app, "widgets"));
fs.writeFileSync(
  path.join(app, "widgets", "widget.js"),
  `import { greet } from "cjs-lib";\nimport { viaWrapper } from "wrapper-lib";\n` +
    `export function run() { document.body.textContent = greet("world") + viaWrapper; }\n`,
);

const get = async (route) => (await fetch(`http://localhost:${port}${route}`)).text();

let server;
let failed = false;
try {
  server = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
  await waitUp(`http://localhost:${port}/`);

  // First serve: the scan never saw the widget, but registration rewrites
  // the bare import straight to the future optimized URL (Vite's optimistic
  // getOptimizedDepPath), never to a per-file /node_modules URL.
  const first = await get("/widgets/widget.js");
  const m = first.match(/\/@oj-deps\/cjs-lib\.mjs\?v=([0-9a-f]{8})/);
  assert.ok(m, `first serve already carries the optimized URL:\n${first}`);
  assert.doesNotMatch(first, /\/node_modules\/cjs-lib\//);

  // The dep route holds the request until the re-optimization commits.
  const bundled = await get(`/@oj-deps/cjs-lib.mjs?v=${m[1]}`);
  assert.match(bundled, /hi /, `the optimized bundle serves once committed:\n${bundled.slice(0, 300)}`);

  // A ?v= the committed entry does not carry is Vite's outdated-request 504.
  const stale = await fetch(`http://localhost:${port}/@oj-deps/cjs-lib.mjs?v=deadbeef`);
  assert.equal(stale.status, 504, "stale version query is an outdated request");

  // The excluded wrapper serves per-file, and the import inside it stays
  // per-file too: a node_modules importer never registers a missing dep.
  const wrapper = await get("/node_modules/wrapper-lib/index.js");
  assert.match(wrapper, /\/node_modules\/inner-lib\//, `excluded dep's import serves per-file:\n${wrapper}`);
  assert.doesNotMatch(wrapper, /@oj-deps\/inner-lib/);

  const manifest = JSON.parse(fs.readFileSync(path.join(app, ".oj-cache", "v1", "deps", "manifest.json"), "utf8"));
  assert.ok(manifest.metadata["cjs-lib"], `cjs-lib in the manifest: ${Object.keys(manifest.metadata)}`);
  assert.deepEqual(manifest.discovered, ["cjs-lib"], "only the user-code discovery is persisted");
  assert.equal(typeof manifest.browserHash, "string", "the rerun version is persisted");

  assert.equal(manifest.metadata["cjs-lib"].v, m[1], "the committed version is the one served first");
  assert.ok(manifest.metadata["cjs-lib"].fileHash, "the entry's content hash is persisted");
  console.log("dep-optimize-discover e2e PASSED");
} catch (err) {
  failed = true;
  console.error("dep-optimize-discover e2e FAILED:", err.message);
} finally {
  if (server) server.kill("SIGKILL");
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
