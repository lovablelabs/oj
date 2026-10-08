// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { runSidecar, rpcSidecar, tmpProject, testWithRolldown, linkRolldown } from "./harness.mjs";

const itRolldown = testWithRolldown(test);

// Shape A: the one-shot helper drives the real optimize-deps sidecar and returns
// its parsed stdout. An explicit `include` is pre-bundled to an emitted file.
// rolldown is the only optimizer engine, so the fixture's rolldown links in.
itRolldown("harness.runSidecar pre-bundles an included dep via optimize-deps", () => {
  const fx = tmpProject({ prefix: "oj-harness-a-" });
  try {
    linkRolldown(fx.root);
    fx.pkg("plaincjs", "index.js", { "index.js": "exports.a = 1;\n" });
    fx.write("entry.js", `import { a } from "plaincjs";\nexport const out = a;\n`);
    const outDir = path.join(fx.root, ".oj-cache", "deps");
    const { metadata } = runSidecar("optimize-deps.mjs", {
      root: fx.root,
      outDir,
      entries: [path.join(fx.root, "entry.js")],
      include: ["plaincjs"],
    });
    assert.deepEqual(Object.keys(metadata), ["plaincjs"]);
    assert.ok(fs.existsSync(path.join(outDir, metadata.plaincjs.file)), "emitted bundle file exists");
  } finally {
    fx.cleanup();
  }
});

// Shape B: the RPC helper drives the real plugin-host over newline-JSON. An
// empty plugins file needs no npm deps; getPluginCount is a hook that never
// triggers a host->driver context request, so a single send/recv round-trips.
test("harness.rpcSidecar round-trips a plugin-host hook", async () => {
  const fx = tmpProject({ prefix: "oj-harness-b-" });
  fx.write("oj.plugins.mjs", "export default [];\n");
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const res = await host.send({ id: 1, hook: "getPluginCount", args: [] });
    assert.equal(res.id, 1, "reply is keyed by the request id");
    assert.equal(res.result, "0", "no plugins loaded from an empty plugins file");
  } finally {
    host.close();
    fx.cleanup();
  }
});
