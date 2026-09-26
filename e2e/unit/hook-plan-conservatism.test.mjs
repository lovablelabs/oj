// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

// The Rust gate may only ever OVER-approximate the host's exact filter
// semantics (Vite's: id strings are cwd-resolved picomatch globs, code
// strings are substring includes, exclude beats include, a present include
// list that does not match rejects). Everything getBuildHookPlan cannot carry
// with byte-identical Rust semantics must therefore pin the plugin
// unfiltered, or the gate under-matches and a hook silently never runs.
test("plan serialization pins everything non-carryable as unfiltered", async () => {
  const fx = tmpProject({ prefix: "oj-hookplan-" });
  fx.write(
    "oj.plugins.mjs",
    `export default [
       // carryable: plain regex include on a load filter
       { name: "clean", load: { filter: { id: /\\.special\\.js$/ }, handler() { return null; } } },
       // string pattern: a glob to rolldown, not a literal; must fail open
       { name: "glob", transform: { filter: { id: "src/**/*.ts" }, handler() { return null; } } },
       // exclude-only filter: the plan carries includes only; must fail open
       { name: "excl", transform: { filter: { id: { exclude: [/skip/] } }, handler() { return null; } } },
       // m flag: JS multiline anchors honor \\r and \\u2028, Rust's (?m) only \\n
       { name: "mflag", transform: { filter: { code: /^use client$/m }, handler() { return null; } } },
       // JS perl classes are ASCII, Rust's Unicode; \\b could under-match
       { name: "word", transform: { filter: { code: /\\bimport\\b/ }, handler() { return null; } } },
       // function form: no filter, always called
       { name: "fn", transform(code) { return null; } },
     ];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const res = await host.send({ id: 1, hook: "getBuildHookPlan", args: [] });
    const plan = JSON.parse(res.result);

    // Every transform plugin above is non-carryable, so the whole transform
    // plan must be unfiltered: the gate lets everything cross.
    assert.equal(plan.transform.present, true);
    assert.equal(plan.transform.unfiltered, true, JSON.stringify(plan.transform));

    // The lone load plugin is carryable: the plan gates on its exact regex.
    assert.equal(plan.load.present, true);
    assert.equal(plan.load.unfiltered, false, JSON.stringify(plan.load));
    assert.equal(plan.load.plugins.length, 1);
    assert.deepEqual(plan.load.plugins[0].id, ["\\.special\\.js$"]);

    // No resolveId hooks at all: the gate may reject every id outright.
    assert.equal(plan.resolveId.present, false);
  } finally {
    host.close();
    fx.cleanup();
  }
});
