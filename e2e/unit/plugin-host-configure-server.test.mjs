// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { rpcSidecar, tmpProject } from "./harness.mjs";

// Vite runs configureServer once per dev server (server/index.ts), on plugin
// instances every environment shares. oj's client and ssr hosts each hold
// their own instances, and both used to run it, so a plugin's server-level
// side effects (a devtools event bus, a watcher) started twice. Only the
// client host, the one oj routes requests to, owns the server now.

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const viteSrc = path.join(repo, "e2e/fixtures/start-app/node_modules/vite");
const haveVite = fs.existsSync(path.join(viteSrc, "package.json"));
const viteTest = haveVite ? test : (name, fn) => test(name, { skip: "fixture vite not installed" }, () => {});

const PLUGINS = `let configured = 0;
export default [
  {
    name: "server-side-effect",
    configureServer(server) {
      configured++;
      server.middlewares.use((req, res, next) => next());
    },
    transform(code, id) {
      if (id.endsWith("probe.js")) return JSON.stringify({ configured });
      return null;
    },
  },
];
`;

async function configuredIn(environment) {
  const fx = tmpProject({ prefix: "oj-configure-server-" });
  fs.symlinkSync(viteSrc, path.join(fx.root, "node_modules", "vite"), "dir");
  fx.write("oj.plugins.mjs", PLUGINS);
  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        config: { root: fx.root },
        env: { command: "serve", mode: "development" },
        environment: { name: environment, mode: "dev" },
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const probe = await host.send({ id: 1, hook: "transform", args: ["", path.join(fx.root, "probe.js"), ""] });
    assert.equal(probe.error, undefined, `transform must not throw: ${probe.error}\n${host.stderr()}`);
    return JSON.parse(JSON.parse(probe.result).code).configured;
  } finally {
    host.close();
    fx.cleanup();
  }
}

viteTest("configureServer runs once, in the client host that owns the dev server", async () => {
  assert.equal(await configuredIn("client"), 1);
  assert.equal(await configuredIn("ssr"), 0, "the ssr host's instances are an environment's, not the server's");
});
