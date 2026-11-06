// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Start's import protection runs as Start ships it, reading Start's config.
// When that config cannot be read (its configResolved throws), a build fails,
// as Vite's would, rather than ship what the plugin keeps out; a dev server
// leaves the pair out of every environment and says so.

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

const PLUGINS = `export default [{
  name: "tanstack-start-core:import-protection",
  enforce: "pre",
  configResolved() { throw new Error("Cannot get config before root is resolved"); },
  resolveId(id) { if (id === "probe") return "\\0protected"; },
}, {
  name: "tanstack-start-core:import-protection-transform-cache",
  transform(code) { return code + "\\n// cache ran"; },
}];
`;

function boot(fx, command, mode) {
  return rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        config: { root: fx.root, base: "/", mode, command },
        env: { command, mode },
        environment: { name: "client", mode: command === "build" ? "build" : "dev" },
        ojStartMode: true,
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
}

test("a build fails when import protection cannot read Start's config", async () => {
  const fx = tmpProject({ prefix: "oj-ip-build-" });
  fx.write("oj.plugins.mjs", PLUGINS);
  const host = boot(fx, "build", "production");
  try {
    // The host's init fails: no init signal, the error on stderr.
    const stderr = await host.waitStderr(/could not read its config/, 30_000);
    assert.match(
      stderr,
      /Start import protection could not read its config: Cannot get config before root is resolved/,
    );
    assert.equal(host.initPushed(), false, "the host finished init");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("a dev server leaves import protection out when it cannot read Start's config", async () => {
  const fx = tmpProject({ prefix: "oj-ip-dev-" });
  fx.write("oj.plugins.mjs", PLUGINS);
  const host = boot(fx, "serve", "development");
  try {
    await host.initSignal();
    const resolved = await host.send({ id: 1, hook: "resolveId", args: ["probe", path.join(fx.root, "a.js"), "{}"] });
    assert.notEqual(JSON.parse(resolved.result ?? "null")?.id, "\0protected", host.stderr());
    const transformed = await host.send({
      id: 2,
      hook: "transform",
      args: ["export {}", path.join(fx.root, "a.js"), ""],
    });
    assert.ok(!String(transformed.result).includes("cache ran"), "the cache plugin still ran");
    assert.match(host.stderr(), /Start import protection is off/);
  } finally {
    host.close();
    fx.cleanup();
  }
});
