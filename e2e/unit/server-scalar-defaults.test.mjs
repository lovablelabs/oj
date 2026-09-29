// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite's resolved `server` guarantees more than `fs`: `_serverConfigDefaults`
// (server/index.ts) gives plugins `port`, `strictPort`, `middlewareMode`,
// `preTransformRequests`, `allowedHosts`, `cors`, `open`, `warmup` and a
// `sourcemapIgnoreList` FUNCTION — the same crash class as the fs.allow
// incident when absent. Present values (including the real port oj's Rust
// side passes at boot) always win; only absent fields get Vite's defaults.
import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { repo, rpcSidecar, tmpProject } from "./harness.mjs";

const bridge = await import(pathToFileURL(join(repo, "crates/oj_server/src/assets/start/vite-plugin-bridge.mjs")).href);

test("bundling container: Vite's server scalar defaults, functions included", async () => {
  let seen = null;
  const container = bridge.createPluginContainer(
    {},
    [
      {
        name: "reader",
        configResolved(config) {
          seen = config.server;
        },
      },
    ],
    { command: "serve", environment: "client", config: { root: repo } },
  );
  await container.resolveId("virtual:probe", undefined);

  assert.equal(seen.port, 5173, "Vite's configured-port default");
  assert.equal(seen.strictPort, false);
  // Vite deliberately leaves an unset host undefined so plugins can detect
  // an explicit one (server/index.ts "do not set here...").
  assert.equal(seen.host, undefined);
  assert.equal(seen.middlewareMode, false);
  assert.equal(seen.perEnvironmentStartEndDuringDev, false);
  assert.equal(seen.perEnvironmentWatchChangeDuringDev, false);
  assert.deepEqual(
    seen.forwardConsole,
    { enabled: false, unhandledErrors: false, logLevels: [] },
    "unset forwardConsole resolves to Vite's disabled shape (oj does not forward)",
  );
  assert.equal(seen.preTransformRequests, true);
  assert.equal(seen.open, false);
  assert.deepEqual(seen.allowedHosts, []);
  assert.deepEqual(seen.warmup, { clientFiles: [], ssrFiles: [] });
  assert.ok(seen.cors.origin instanceof RegExp, "cors.origin is Vite's loopback regex");
  assert.ok(seen.cors.origin.test("http://localhost:5173"));
  assert.ok(!seen.cors.origin.test("http://evil.example"));
  assert.equal(typeof seen.sourcemapIgnoreList, "function");
  assert.equal(seen.sourcemapIgnoreList("/x/node_modules/y.js"), true, "default is isInNodeModules");
  assert.equal(seen.sourcemapIgnoreList("/src/a.js"), false);
});

test("bundling container: user values win, object forms deep-fill", async () => {
  let seen = null;
  const container = bridge.createPluginContainer(
    {},
    [
      {
        name: "reader",
        configResolved(config) {
          seen = config.server;
        },
      },
    ],
    {
      command: "serve",
      environment: "client",
      config: {
        root: repo,
        server: {
          port: 4000,
          cors: false,
          warmup: { clientFiles: ["./a.js"] },
          sourcemapIgnoreList: false,
        },
      },
    },
  );
  await container.resolveId("virtual:probe", undefined);

  assert.equal(seen.port, 4000, "a configured port is never replaced");
  assert.equal(seen.cors, false, "a non-object cors value is carried as-is");
  assert.deepEqual(seen.warmup, { clientFiles: ["./a.js"], ssrFiles: [] }, "warmup deep-fills like mergeWithDefaults");
  assert.equal(seen.sourcemapIgnoreList("/x/node_modules/y.js"), false, "false resolves to a constant-false function");
});

test("bundling container: forwardConsole true and object forms resolve like Vite", async () => {
  const resolved = async (forwardConsole) => {
    let seen = null;
    const container = bridge.createPluginContainer(
      {},
      [
        {
          name: "reader",
          configResolved(config) {
            seen = config.server.forwardConsole;
          },
        },
      ],
      { command: "serve", environment: "client", config: { root: repo, server: { forwardConsole } } },
    );
    await container.resolveId("virtual:probe", undefined);
    return seen;
  };
  assert.deepEqual(await resolved(true), { enabled: true, unhandledErrors: true, logLevels: ["error", "warn"] });
  assert.deepEqual(await resolved({ logLevels: ["error"] }), {
    enabled: true,
    unhandledErrors: true,
    logLevels: ["error"],
  });
  assert.deepEqual(await resolved({ unhandledErrors: false }), {
    enabled: false,
    unhandledErrors: false,
    logLevels: [],
  });
});

test("bundling container: ws/hmr compat, allowedHosts env append, origin strip", async () => {
  const resolved = async (server, env) => {
    if (env) process.env.__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS = env;
    try {
      let seen = null;
      const container = bridge.createPluginContainer(
        {},
        [
          {
            name: "reader",
            configResolved(config) {
              seen = config.server;
            },
          },
        ],
        { command: "serve", environment: "client", config: { root: repo, server } },
      );
      await container.resolveId("virtual:probe", undefined);
      return seen;
    } finally {
      if (env) delete process.env.__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS;
    }
  };

  // setupHmrWsOptionCompat: resolved ws is ALWAYS an object unless disabled,
  // legacy hmr keys mirrored where ws leaves them unset, hmr:true -> {}.
  assert.deepEqual((await resolved({})).ws, {}, "ws defaults to an object");
  const mirrored = await resolved({ hmr: { port: 24678, protocol: "wss" }, ws: { port: 1234 } });
  assert.deepEqual(mirrored.ws, { port: 1234, protocol: "wss" }, "hmr keys fill only unset ws keys");
  assert.deepEqual((await resolved({ hmr: true })).hmr, {}, "hmr:true normalizes to {}");
  assert.equal((await resolved({ ws: false })).ws, false, "ws:false is respected");
  assert.equal((await resolved({ hmr: false })).ws, undefined, "hmr:false skips the compat entirely");

  // __VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS: comma-split, trimmed, appended;
  // reserved characters skip the whole value.
  assert.deepEqual((await resolved({}, " a.example ,b.example,")).allowedHosts, ["a.example", "b.example"]);
  assert.deepEqual((await resolved({}, 'bad"host')).allowedHosts, [], "reserved characters skip the append");

  // origin: a single trailing slash is stripped.
  assert.equal((await resolved({ origin: "http://x:8080/" })).origin, "http://x:8080");
});

test("dev plugin host: the boot config's real port wins over Vite's default", async () => {
  const fx = tmpProject({ prefix: "oj-scalar-" });
  fx.write(
    "oj.plugins.mjs",
    `let seen = {};
     export default [{
       name: "reader",
       configResolved(config) {
         seen = {
           port: config.server.port,
           strictPort: config.server.strictPort,
           preTransformRequests: config.server.preTransformRequests,
           middlewareMode: config.server.middlewareMode,
           ignoreDep: config.server.sourcemapIgnoreList("/x/node_modules/y.js"),
           ignoreSrc: config.server.sourcemapIgnoreList("/src/a.js"),
         };
       },
       transform(code, id) {
         if (id.endsWith("probe.js")) return "export default " + JSON.stringify(seen) + ";";
         return null;
       },
     }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        // The shape Rust sends: the REAL oj port and strictPort at boot.
        config: { root: fx.root, server: { port: 5199, strictPort: true } },
        env: { command: "serve", mode: "development" },
        environment: { name: "client" },
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const res = await host.send({
      id: 1,
      hook: "transform",
      args: ["", path.join(fx.root, "probe.js")],
    });
    const seen = JSON.parse(
      JSON.parse(res.result)
        .code.replace(/^export default /, "")
        .replace(/;$/, ""),
    );
    assert.equal(seen.port, 5199, "oj's real configured port, never 5173");
    assert.equal(seen.strictPort, true, "oj's strictPort, not the default");
    assert.equal(seen.preTransformRequests, true);
    assert.equal(seen.middlewareMode, false);
    assert.equal(seen.ignoreDep, true);
    assert.equal(seen.ignoreSrc, false);
  } finally {
    host.close();
    fx.cleanup();
  }
});
