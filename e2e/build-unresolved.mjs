// SPDX-License-Identifier: MIT

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const binary = path.join(root, "target", "debug", "oj");
const project = fs.mkdtempSync(path.join(os.tmpdir(), "oj-unresolved-"));
const outside = fs.mkdtempSync(path.join(os.tmpdir(), "oj-unresolved-outside-"));

try {
  fs.writeFileSync(path.join(project, "package.json"), JSON.stringify({ type: "module" }));
  fs.writeFileSync(
    path.join(project, "index.html"),
    '<html><body><script type="module" src="/main.js"></script></body></html>',
  );
  fs.writeFileSync(
    path.join(project, "main.js"),
    'import "missing-example-dependency"; document.body.textContent = "ready";',
  );

  const missing = spawnSync(binary, ["build", project], { cwd: root, encoding: "utf8" });
  assert.notEqual(
    missing.status,
    0,
    `an unresolved dependency must fail the build:\n${missing.stdout}\n${missing.stderr}`,
  );
  const missingOut = `${missing.stdout}\n${missing.stderr}`;
  assert.match(missingOut, /missing-example-dependency/);
  // Like Vite, the failure names the escape hatch for a deliberate external.
  assert.match(
    missingOut,
    /build\.rollupOptions\.external/,
    `the failure must point at the externalization option:\n${missingOut}`,
  );
  // Like Vite (which writes nothing), a failed build leaves no broken bundle on disk.
  assert.equal(
    fs.existsSync(path.join(project, "dist")),
    false,
    "a failed build must not leave a partial dist/ behind",
  );

  // An outDir outside the project root was never oj's to empty (Vite's emptyOutDir
  // rule), so the failed-build cleanup must not start clearing it either.
  fs.writeFileSync(path.join(outside, "keep.txt"), "not yours to delete");
  fs.writeFileSync(path.join(project, "oj.config.json"), JSON.stringify({ build: { outDir: outside } }));
  const outsideRun = spawnSync(binary, ["build", project], { cwd: root, encoding: "utf8" });
  assert.notEqual(outsideRun.status, 0, "the unresolved import must still fail the build with an outside outDir");
  assert.equal(
    fs.readFileSync(path.join(outside, "keep.txt"), "utf8"),
    "not yours to delete",
    "a failed build must not wipe an outDir outside the project root",
  );

  fs.writeFileSync(
    path.join(project, "oj.config.json"),
    JSON.stringify({
      build: { rollupOptions: { external: ["missing-example-dependency"] } },
    }),
  );
  const external = spawnSync(binary, ["build", project], { cwd: root, encoding: "utf8" });
  assert.equal(
    external.status,
    0,
    `an explicitly external dependency must remain allowed:\n${external.stdout}\n${external.stderr}`,
  );

  console.log("BUILD-UNRESOLVED E2E PASSED");
} finally {
  fs.rmSync(project, { recursive: true, force: true });
  fs.rmSync(outside, { recursive: true, force: true });
}
