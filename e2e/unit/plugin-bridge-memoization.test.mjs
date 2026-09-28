// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// The bridge memoizes what Vite fixes at container build: applyToEnvironment
// per plugin and environment (resolveEnvironmentPlugins evaluates it once),
// the per-hook plugin order (createPluginHookUtils' sortedPluginsCache), and
// the per-phase resolveId candidate lists. These tests pin the parts a stale
// or colliding cache would break silently.

import { test } from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { repo } from "./harness.mjs";

const bridge = await import(
  pathToFileURL(join(repo, "crates/oj_server/src/assets/start/vite-plugin-bridge.mjs")).href
);

const makeContainer = (plugins) =>
  bridge.createPluginContainer({}, plugins, {
    command: "serve",
    environment: "ssr",
    config: { root: repo, environments: { client: {}, ssr: {} } },
  });

test("applyToEnvironment is evaluated once per environment across every resolve path", async () => {
  // Vite runs applyToEnvironment ONCE per environment when it builds the
  // environment's plugin list (resolveEnvironmentPlugins); the answer is
  // fixed. A stateful hook must therefore never be re-consulted, and the
  // memo must survive the host-driven path and both bundler phases.
  let calls = 0;
  const plugins = [
    {
      name: "count-ate",
      applyToEnvironment() {
        calls++;
        return true;
      },
      resolveId(id) {
        return id === "count-probe" ? "\0count-probe" : null;
      },
    },
  ];
  const container = makeContainer(plugins);
  for (let i = 0; i < 5; i++) {
    const r = await container.resolveIdResult("count-probe", undefined, null);
    assert.equal(r?.id, "\0count-probe");
  }
  await container.resolveIdResult("count-probe", undefined, null, "pre");
  await container.resolveIdResult("count-probe", undefined, null, "post");
  assert.equal(calls, 1, "applyToEnvironment must be consulted exactly once per environment");
});

test("oj-reimplemented plugins answer host-driven resolves but never bundler-driven phases", async () => {
  // The candidate cache keys on phase + bundler-driven-ness: a plugin oj
  // reimplements natively (tanstack-*, vite:*) answers the host's own resolve
  // path but stays out of the bundle's pre/post routing (oj's native pass
  // covers it). A cache-key collision would leak the wrong list; the repeat
  // runs exercise the memoized path against the first-call path.
  const plugins = [
    {
      name: "tanstack-probe",
      enforce: "pre",
      resolveId(id) {
        return id === "reimpl-probe" ? "\0reimpl" : null;
      },
    },
  ];
  const container = makeContainer(plugins);
  for (let run = 0; run < 2; run++) {
    const dev = await container.resolveIdResult("reimpl-probe", undefined, null);
    assert.equal(dev?.id, "\0reimpl", `run ${run}: the host's own resolve path consults it`);
    for (const phase of ["pre", "post"]) {
      const r = await container.resolveIdResult("reimpl-probe", undefined, null, phase);
      assert.equal(r, null, `run ${run}, ${phase}: bundler-driven phases skip oj-reimplemented plugins`);
    }
  }
});

test("an async applyToEnvironment counts as allowed (pinned deviation: Vite awaits it)", async () => {
  // Vite's resolveEnvironmentPlugins AWAITS applyToEnvironment; the bridge's
  // sync paths cannot, so a thenable is treated as allowed. This pins the
  // deviation: if the bridge ever awaits it, flip this test deliberately.
  const plugins = [
    {
      name: "async-ate",
      async applyToEnvironment() {
        return false;
      },
      resolveId(id) {
        return id === "async-ate-probe" ? "\0async-ate" : null;
      },
    },
  ];
  const container = makeContainer(plugins);
  const r = await container.resolveIdResult("async-ate-probe", undefined, null);
  assert.equal(r?.id, "\0async-ate", "a thenable applyToEnvironment is treated as allowed");
});
