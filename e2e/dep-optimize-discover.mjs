// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Runtime dep discovery (Vite's registerMissingImport): a module only
// reachable through a runtime-built import() is invisible to the scan, so its
// bare dep starts out served per-file. Serving it registers the dep, a
// debounced re-optimization bundles it, and the module re-serves rewritten to
// the versioned /@oj-deps URL, with the discovery persisted in the manifest.

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
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "optdep-disc-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/main.js"></script></body></html>`,
);
// The widget is reached only through a runtime-built specifier: the scan
// cannot follow it, so cjs-lib is not in the initial pre-bundle.
fs.writeFileSync(
  path.join(app, "main.js"),
  `const name = "wid" + "get";\nimport("/widgets/" + name + ".js").then((m) => m.run());\n`,
);
fs.mkdirSync(path.join(app, "widgets"));
fs.writeFileSync(
  path.join(app, "widgets", "widget.js"),
  `import { greet } from "cjs-lib";\nexport function run() { document.body.textContent = greet("world"); }\n`,
);

const get = async (route) => (await fetch(`http://localhost:${port}${route}`)).text();

let server;
let failed = false;
try {
  server = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
  await waitUp(`http://localhost:${port}/`);

  // First serve: the dep is not pre-bundled (the scan never saw the widget),
  // so the import stays per-file; serving it registers the missing dep.
  const first = await get("/widgets/widget.js");
  assert.match(first, /\/node_modules\/cjs-lib\//, `dep starts unbundled:\n${first}`);
  assert.doesNotMatch(first, /@oj-deps\/cjs-lib/);

  // The debounced rerun commits, caches invalidate, and the module re-serves
  // against the new map with a versioned optimized URL.
  let widget = "";
  const rebundled = await settles(
    async () => {
      widget = await get("/widgets/widget.js");
      return /\/@oj-deps\/cjs-lib\.mjs\?v=[0-9a-f]{8}/.test(widget);
    },
    { pollMs: 250 },
  );
  assert.ok(rebundled, `widget still imports the unbundled dep:\n${widget}`);

  const manifest = JSON.parse(fs.readFileSync(path.join(app, ".oj-cache", "v1", "deps", "manifest.json"), "utf8"));
  assert.ok(manifest.metadata["cjs-lib"], `cjs-lib in the manifest: ${Object.keys(manifest.metadata)}`);
  assert.deepEqual(manifest.discovered, ["cjs-lib"], "the discovery is persisted for the next boot");
  assert.equal(typeof manifest.browserHash, "string", "the rerun version is persisted");

  const dep = await get(`/@oj-deps/${manifest.metadata["cjs-lib"].file}?v=${manifest.browserHash}`);
  assert.match(dep, /hi /, "the optimized bundle serves");
  console.log("dep-optimize-discover e2e PASSED");
} catch (err) {
  failed = true;
  console.error("dep-optimize-discover e2e FAILED:", err.message);
} finally {
  if (server) server.kill("SIGKILL");
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
