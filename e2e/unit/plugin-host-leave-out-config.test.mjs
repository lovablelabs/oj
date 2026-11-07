// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// A container that leaves the framework plugins oj reimplements out (a Start
// SSR or client-bundle container on the dev host) reads its own values from
// the config the remaining plugins make, replayed from their first run. Vite
// honors a config hook's in-place edits as much as what it returns (its
// runConfigHook merges a returned partial into the config it handed out), and
// the same for configEnvironment: the replay must reproduce both.

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { repo, rpcSidecar, tmpProject } from "./harness.mjs";

const viteSrc = path.join(repo, "e2e/fixtures/start-app/node_modules/vite");

const PLUGINS = `[
  {
    name: "edits-in-place",
    config(config) {
      config.define ??= {};
      config.define.__CONFIG_MUT__ = JSON.stringify("config-in-place");
      config.environments ??= {};
      config.environments.ssr ??= {};
      config.environments.ssr.define = { __SSR_MUT__: JSON.stringify("env-in-place") };
    },
    configEnvironment(name, options) {
      if (name !== "ssr") return;
      options.define ??= {};
      options.define.__ENV_HOOK_MUT__ = JSON.stringify("configEnvironment-in-place");
    },
  },
  {
    name: "returns",
    config: () => ({ define: { __RETURNED__: JSON.stringify("returned") } }),
  },
  // Left out of the container: its contributions must not reach it.
  { name: "tanstack-start-core:config", config: () => ({ define: { __FRAMEWORK__: "1" } }) },
]`;

async function ssrDefines(fx, file, pluginsFormat) {
  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, file),
      JSON.stringify({
        pluginsFormat,
        config: { root: fx.root, base: "/", mode: "development", command: "serve" },
        env: { command: "serve", mode: "development" },
        environment: { name: "client", mode: "dev" },
        ojStartMode: true,
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  const call = async (id, hook, args, container) => {
    const r = await host.send({ id, hook, args, container });
    assert.equal(r.error, undefined, `${hook}: ${r.error}\n${host.stderr()}`);
    return r.result;
  };
  try {
    await host.initSignal();
    const ssr = Number(
      await call(1, "openEnvironmentContainer", [JSON.stringify({ id: 1 << 20, name: "ssr", leaveOut: true })]),
    );
    return JSON.parse(await call(2, "getPluginConfig", [], ssr)).define ?? {};
  } finally {
    host.close();
  }
}

const expected = {
  __CONFIG_MUT__: JSON.stringify("config-in-place"),
  __SSR_MUT__: JSON.stringify("env-in-place"),
  __ENV_HOOK_MUT__: JSON.stringify("configEnvironment-in-place"),
  __RETURNED__: JSON.stringify("returned"),
};

test("a leave-out container replays the hooks' in-place edits (oj plugins file)", async () => {
  const fx = tmpProject({ prefix: "oj-leave-out-oj-" });
  fx.write("oj.plugins.mjs", `export default ${PLUGINS};\n`);
  try {
    const define = await ssrDefines(fx, "oj.plugins.mjs", "oj");
    assert.deepEqual(define, expected);
  } finally {
    fx.cleanup();
  }
});

test(
  "a leave-out container replays the hooks' in-place edits (vite.config)",
  { skip: !fs.existsSync(viteSrc) && "fixture vite not installed" },
  async () => {
    const fx = tmpProject({ prefix: "oj-leave-out-vite-" });
    fs.symlinkSync(viteSrc, path.join(fx.root, "node_modules", "vite"), "dir");
    fx.write("vite.config.mjs", `export default { plugins: ${PLUGINS} };\n`);
    try {
      const define = await ssrDefines(fx, "vite.config.mjs", "vite");
      assert.deepEqual(define, expected);
    } finally {
      fx.cleanup();
    }
  },
);

// A hook that edits an array in place records what it added, not the whole
// array: items a left-out plugin put there before it stay out.
test("a leave-out container's array edits carry no left-out plugin's items", async () => {
  const fx = tmpProject({ prefix: "oj-leave-out-array-" });
  fx.write(
    "oj.plugins.mjs",
    `export default [
  { name: "tanstack-start-core:config", config: () => ({ build: { rollupOptions: { external: ["framework"] } } }) },
  {
    name: "pushes",
    config(config) {
      config.build.rollupOptions.external.push("user");
      config.build.rollupOptions.external.unshift("first");
    },
  },
];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        pluginsFormat: "oj",
        config: { root: fx.root, base: "/", mode: "development", command: "serve" },
        env: { command: "serve", mode: "development" },
        environment: { name: "client", mode: "dev" },
        ojStartMode: true,
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    await host.initSignal();
    const opened = await host.send({
      id: 1,
      hook: "openEnvironmentContainer",
      args: [JSON.stringify({ id: 1 << 20, name: "ssr", leaveOut: true })],
    });
    assert.equal(opened.error, undefined, host.stderr());
    const r = await host.send({ id: 2, hook: "startFacts", args: [], container: Number(opened.result) });
    assert.equal(r.error, undefined, host.stderr());
    assert.deepEqual(JSON.parse(r.result).external, ["first", "user"]);
  } finally {
    host.close();
    fx.cleanup();
  }
});
