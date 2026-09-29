// SPDX-License-Identifier: MIT

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const binary = path.join(root, "target", "debug", "oj");
const project = fs.mkdtempSync(path.join(os.tmpdir(), "oj-config-failure-"));

try {
  fs.writeFileSync(path.join(project, "package.json"), JSON.stringify({ type: "module" }));
  fs.writeFileSync(
    path.join(project, "index.html"),
    '<html><body><script type="module" src="/main.js"></script></body></html>',
  );
  fs.writeFileSync(path.join(project, "main.js"), 'document.body.textContent = "ready";');
  fs.writeFileSync(path.join(project, "vite.config.mjs"), 'import "missing-config-plugin"; export default {};\n');

  const build = spawnSync(binary, ["build", project], { cwd: root, encoding: "utf8" });
  assert.notEqual(
    build.status,
    0,
    `a broken Vite config must fail instead of being ignored:\n${build.stdout}\n${build.stderr}`,
  );
  assert.match(`${build.stdout}\n${build.stderr}`, /missing-config-plugin/);

  console.log("CONFIG-LOAD-FAILURE E2E PASSED");
} finally {
  fs.rmSync(project, { recursive: true, force: true });
}
