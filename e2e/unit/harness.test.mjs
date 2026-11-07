// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { runSidecar, rpcSidecar, tmpProject } from "./harness.mjs";

// Shape A: the one-shot helper drives a real oj-owned module's export and
// returns its parsed result: dep-names reads a CJS dep's runtime names.
test("harness.runSidecar runs a one-shot module's export", () => {
  const fx = tmpProject({ prefix: "oj-harness-a-" });
  try {
    fx.pkg("plaincjs", "index.js", { "index.js": "exports.a = 1;\nexports.b = 2;\n" });
    const names = runSidecar(
      "dep-names.mjs",
      { root: fx.root, deps: ["plaincjs", "missing"] },
      { exportName: "namedExports" },
    );
    assert.deepEqual(names, { plaincjs: ["a", "b"], missing: [] });
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
