// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { globFiles, idFilterRegExp, loadViteGlobs } from "../../crates/oj_server/src/assets/start/vite-globs.mjs";

// An app with no Vite installed (a plugin config run directly, a plain app's
// dep optimizer) gets the picomatch and tinyglobby releases oj ships, and
// they answer as Vite's do.
test("without a Vite install, the shipped picomatch and tinyglobby stand in", async () => {
  const root = mkdtempSync(join(tmpdir(), "oj-vite-globs-"));
  try {
    for (const f of ["x/a/m.json", "x/c/m.json", ".hidden/m.json"]) {
      mkdirSync(join(root, f, ".."), { recursive: true });
      writeFileSync(join(root, f), "{}");
    }
    const { picomatch, tinyglobby } = await loadViteGlobs(root);
    assert.equal(typeof picomatch.scan, "function");
    assert.equal(typeof tinyglobby.escapePath, "function");
    assert.deepEqual(globFiles([`${root}/x/[ab]/*.json`], { cwd: root }), [join(root, "x/a/m.json")]);
    assert.deepEqual(globFiles([`${root}/**/m.json`], { cwd: root }).length, 2, "dot parts stay hidden");
    assert.ok(idFilterRegExp("**/*.{ts,tsx}", root).test("/app/src/.cache/a.tsx"), "filter ids match with dot: true");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
