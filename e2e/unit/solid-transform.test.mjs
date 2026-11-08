// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// solid-transform.mjs helpers: React apps keep the historical automatic-JSX
// path and untouched conditions; Solid apps (solid-js in dependencies) get
// rolldown JSX preservation (babel-preset-solid owns the transform),
// the `solid` condition first, and a babel-backed bundle plugin.
import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const assets = fileURLToPath(new URL("../../crates/oj_server/src/assets/start/", import.meta.url));
const load = (name) => import(new URL(name, `file://${assets}`).href);

import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

function makeApp(pkg) {
  const dir = mkdtempSync(join(tmpdir(), "oj-solid-transform-"));
  writeFileSync(join(dir, "package.json"), JSON.stringify(pkg));
  return dir;
}

test("non-solid apps keep the react-automatic bundle path", async () => {
  const { solidJsxOption, solidConditions, solidBundlePlugin } = await load("solid-transform.mjs");
  const dir = makeApp({ name: "plain", dependencies: { react: "19" } });
  try {
    const fallback = { runtime: "automatic" };
    assert.equal(solidJsxOption(dir, fallback), fallback, "jsx options pass through");
    const conds = ["browser", "import"];
    assert.equal(solidConditions(conds, dir), conds, "conditions pass through untouched");
    assert.equal(await solidBundlePlugin(dir, "dom"), null, "no bundle plugin without solid");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("solid apps get preserve-jsx, the solid condition, and a bundle plugin", async () => {
  const { solidJsxOption, solidConditions, solidBundlePlugin } = await load("solid-transform.mjs");
  const dir = makeApp({ name: "solid-app", dependencies: { "solid-js": "^1.0.0" } });
  try {
    assert.equal(solidJsxOption(dir, { runtime: "automatic" }), "preserve");
    assert.deepEqual(solidConditions(["browser", "import"], dir), ["solid", "browser", "import"]);
    assert.deepEqual(
      solidConditions(["solid", "browser"], dir),
      ["solid", "browser"],
      "an existing solid condition is not duplicated",
    );
    // The toolchain ships with vite-plugin-solid; if it resolves here the
    // plugin builds, otherwise null (never throws for missing toolchain).
    const plugin = await solidBundlePlugin(dir, "dom");
    if (plugin !== null) {
      assert.equal(plugin.name, "oj-solid-transform");
      assert.equal(await plugin.transform("export const x = 1;", "plain.ts"), null, "non-jsx passes through");
      const out = await plugin.transform("export const A = () => <div>hi</div>;", "A.tsx");
      assert.match(out.code, /_tmpl\$|createComponent|jsx/, "tsx compiles through babel-preset-solid");
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("start-pkg defaults to react-start outside a configured run", async () => {
  const { startPkg, routerTarget, clientEntryFile, serverEntryFile } = await load("start-pkg.mjs");
  assert.equal(startPkg(), "@tanstack/react-start");
  assert.equal(routerTarget(), "react");
  assert.equal(clientEntryFile(), "client-entry.tsx");
  assert.equal(serverEntryFile(), "server-entry.tsx");
});
