// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// esmEntry: the file Node's `import(name)` loads, from the path
// `require.resolve(name)` gave, through the package's exports under Node's
// import conditions (`node`, `import`, `default`, in the map's key order).

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { esmEntry } from "../../crates/oj_server/src/assets/vite-config-node.mjs";

function pkg(exportsField, extra = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oj-esm-entry-"));
  fs.writeFileSync(path.join(dir, "package.json"), JSON.stringify({ name: "vite", exports: exportsField, ...extra }));
  fs.mkdirSync(path.join(dir, "dist"), { recursive: true });
  return { dir, cjs: path.join(dir, "index.cjs") };
}

test("Vite 5/6: the import condition, not the CJS proxy require picked", () => {
  const { dir, cjs } = pkg({
    ".": { import: { types: "./x.d.ts", default: "./dist/node/index.js" }, require: { default: "./index.cjs" } },
    "./package.json": "./package.json",
  });
  assert.equal(esmEntry(cjs, "vite"), path.join(dir, "dist/node/index.js"));
});

test("Vite 8: a string export", () => {
  const { dir } = pkg("./dist/node/index.js");
  assert.equal(esmEntry(path.join(dir, "dist/node/index.js"), "vite"), path.join(dir, "dist/node/index.js"));
});

test("conditions in the map's own order, nested and arrays", () => {
  const { dir, cjs } = pkg({ ".": [{ worker: "./w.js", default: { node: "./n.js", import: "./i.js" } }] });
  assert.equal(esmEntry(cjs, "vite"), path.join(dir, "n.js"));
});

test("no exports, or another package's file, keeps what require found", () => {
  const { cjs } = pkg(undefined, { main: "index.cjs" });
  assert.equal(esmEntry(cjs, "vite"), cjs);
  assert.equal(esmEntry(cjs, "other"), cjs);
});
