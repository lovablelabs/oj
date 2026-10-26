// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

// Regression for #37: oj must run `configResolved` before it evaluates
// `applyToEnvironment`, matching Vite (config.ts resolves environment plugins
// "after configResolved because there are downstream projects modifying the
// plugins in it"). Plugins such as @tanstack/router-plugin populate a closure
// variable in `configResolved` and read it from `applyToEnvironment`.
test("configResolved runs before applyToEnvironment so the plugin stays active", async () => {
  const fx = tmpProject({ prefix: "oj-ate-" });
  // `applyToEnvironment` returns false until `configResolved` has flipped the
  // flag; if oj evaluated it first the plugin would be filtered out and its
  // transform would never run.
  fx.write(
    "oj.plugins.mjs",
    `let ready = false;
     export default [{
       name: "order-sensitive",
       configResolved() { ready = true; },
       applyToEnvironment() { return ready; },
       transform(code, id) {
         return id.endsWith("target.js") ? { code: code + "\\n/*ACTIVE*/", map: null } : null;
       },
     }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const res = await host.send({
      id: 1,
      hook: "transform",
      args: ["let a = 1;", path.join(fx.root, "target.js")],
    });
    const out = JSON.parse(res.result);
    assert.match(out.code, /ACTIVE/, "the plugin was applied to the environment after configResolved ran");
  } finally {
    host.close();
    fx.cleanup();
  }
});

// The exact @tanstack/router-plugin shape: `applyToEnvironment` reads
// `userConfig.plugin?...` where `userConfig` is only assigned in
// `configResolved`. With the correct order it never throws.
test("a tanstack-style applyToEnvironment reading configResolved state does not throw", async () => {
  const fx = tmpProject({ prefix: "oj-ate-" });
  fx.write(
    "oj.plugins.mjs",
    `let userConfig;
     export default [{
       name: "tanstack-like",
       configResolved() { userConfig = { plugin: { vite: {} } }; },
       applyToEnvironment(environment) {
         if (userConfig.plugin?.vite?.environmentName)
           return userConfig.plugin.vite.environmentName === environment.name;
         return true;
       },
       transform(code, id) {
         return id.endsWith("target.js") ? { code: code + "\\n/*ACTIVE*/", map: null } : null;
       },
     }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const res = await host.send({
      id: 1,
      hook: "transform",
      args: ["let a = 1;", path.join(fx.root, "target.js")],
    });
    const out = JSON.parse(res.result);
    assert.match(out.code, /ACTIVE/, "the plugin loaded and applied without a TypeError");
  } finally {
    host.close();
    fx.cleanup();
  }
});

// A plugin whose applyToEnvironment genuinely throws rejects Vite's
// createServer (the dev boot fails, tests/dev plugin_environments.rs); Start
// mode tolerates a framework config's lifecycle hooks per plugin instead: oj
// warns, keeps the plugin active, and the host keeps serving RPC.
test("in Start mode a throwing applyToEnvironment keeps the plugin, the host serving", async () => {
  const fx = tmpProject({ prefix: "oj-ate-" });
  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "throws-in-ate",
       applyToEnvironment() { throw new Error("boom"); },
       transform(code, id) {
         return id.endsWith("target.js") ? { code: code + "\\n/*ACTIVE*/", map: null } : null;
       },
     }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root, ojStartMode: true })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const first = await host.send({
      id: 1,
      hook: "transform",
      args: ["let a = 1;", path.join(fx.root, "target.js")],
    });
    assert.match(JSON.parse(first.result).code, /ACTIVE/, "the plugin stayed active despite throwing");
    const second = await host.send({
      id: 2,
      hook: "transform",
      args: ["let b = 2;", path.join(fx.root, "other.js")],
    });
    assert.equal(second.id, 2, "the host still answers a second request");
  } finally {
    host.close();
    fx.cleanup();
  }
});
