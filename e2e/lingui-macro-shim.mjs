// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// End-to-end: when an app imports the @lingui/*/macro entrypoints, the real oj
// binary warns on stderr that it is serving the runtime identity shim. The
// shim route, the import rewrite and every shim export are covered by the
// in-process dev tests; this script keeps only the process stderr check.

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
const port = 5291;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-lingui-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "lingui-shim-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>lingui</title></head><body><div id="root"></div><script type="module" src="/main.js"></script></body></html>`,
);
// Imports all three macro entrypoints. None of the @lingui packages are
// installed: oj must intercept the specifiers by name and serve the shim.
fs.writeFileSync(
  path.join(app, "main.js"),
  `import { t } from "@lingui/core/macro";
import { Trans } from "@lingui/react/macro";
import { t as ut } from "@lingui/macro";
window.__OJ_LINGUI = [t, Trans, ut];
`,
);

let server;
let stderr = "";
let failed = false;
try {
  server = spawn(oj, ["dev", app, "--port", String(port)], { stdio: ["ignore", "ignore", "pipe"] });
  server.stderr.on("data", (d) => (stderr += d.toString()));
  await waitUp(`http://localhost:${port}/`);

  // Transforming main.js rewrites the macro imports, which is what warns.
  const res = await fetch(`http://localhost:${port}/main.js`);
  assert.equal(res.status, 200);
  await res.text();

  assert.match(stderr, /runtime identity shim/, "oj warns once about the degraded shim");

  console.log("PASS lingui-macro-shim");
} catch (e) {
  failed = true;
  console.error("FAIL lingui-macro-shim:", e.message);
  if (stderr) console.error("--- oj stderr ---\n" + stderr.slice(0, 2000));
} finally {
  if (server) server.kill();
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
