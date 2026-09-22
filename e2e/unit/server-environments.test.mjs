// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

// Vite's createServer always exposes `server.environments` with client and ssr
// (server/index.ts builds a DevEnvironment per config.environments entry),
// `server.httpServer` is a listening Node server whose address() is the dev
// port, and `moduleGraph.getModuleById` is undefined for ids it never saw. The
// host used to leave environments undefined unless the Cloudflare plugin was
// present, httpServer null, and mint a graph node for any id.
test("configureServer sees client/ssr environments, an addressable httpServer and a Vite-shaped module graph", async () => {
  const fx = tmpProject({ prefix: "oj-srvenv-" });
  fx.write("src/real.js", "export const R = 1;\n");
  const real = path.join(fx.root, "src", "real.js");
  fx.write(
    "oj.plugins.mjs",
    `let seen = { listening: false };
     export default [{
       name: "env-reader",
       configureServer(server) {
         const envs = server.environments;
         seen.envNames = Object.keys(envs).sort();
         seen.clientConsumer = envs.client.config.consumer;
         seen.ssrConsumer = envs.ssr.config.consumer;
         seen.ssrName = envs.ssr.name;
         seen.clientSharesGraph = envs.client.moduleGraph === server.moduleGraph;
         seen.ssrOwnGraph = envs.ssr.moduleGraph !== server.moduleGraph;
         seen.ssrHasPlugins = Array.isArray(envs.ssr.plugins);
         seen.ssrContainer = typeof envs.ssr.pluginContainer.resolveId === "function";
         seen.address = server.httpServer.address();
         seen.urlsAtConfigure = server.resolvedUrls;
         server.httpServer.once("listening", () => {
           seen.listening = true;
           seen.addressAtListening = server.httpServer.address();
           seen.urlsAtListening = server.resolvedUrls;
         });
         seen.unknownIsUndefined = server.moduleGraph.getModuleById("\\0never-seen") === undefined;
         seen.unknownFileIsUndefined = server.moduleGraph.getModuleById(${JSON.stringify(path.join(fx.root, "src", "missing.js"))}) === undefined;
         const node = server.moduleGraph.getModuleById(${JSON.stringify(real)});
         seen.realFileIsNode = !!node && node.file === ${JSON.stringify(real)};
         seen.fileMapPopulated = server.moduleGraph.fileToModulesMap.get(${JSON.stringify(real)})?.has(node) === true;
         seen.unknownFileSet = server.moduleGraph.getModulesByFile("/nope/never.js") === undefined;
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
        config: { root: fx.root, server: { port: 6402, host: null } },
        env: { command: "serve", mode: "development" },
        environment: { name: "client", mode: "dev" },
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const res = await host.send({ id: 1, hook: "transform", args: ["", path.join(fx.root, "probe.js"), ""] });
    assert.equal(res.error, undefined, `configureServer must not throw: ${res.error}\n${host.stderr()}`);
    const seen = JSON.parse(JSON.parse(res.result).code.replace(/^export default /, "").replace(/;$/, ""));
    assert.deepEqual(seen.envNames, ["client", "ssr"], "client and ssr are always present");
    assert.equal(seen.clientConsumer, "client");
    assert.equal(seen.ssrConsumer, "server", "the ssr environment's config is tagged consumer: server");
    assert.equal(seen.ssrName, "ssr");
    assert.equal(seen.clientSharesGraph, true, "environments.client.moduleGraph is server.moduleGraph");
    assert.equal(seen.ssrOwnGraph, true, "the ssr environment has its own graph");
    assert.equal(seen.ssrHasPlugins, true);
    assert.equal(seen.ssrContainer, true, "environments carry a pluginContainer");
    // Vite parity: during configureServer the socket is not bound yet, so
    // address() is null (Vite's listen() runs after createServer); it carries
    // the dev port only once "listening" has fired.
    assert.equal(seen.address, null, "httpServer.address() is null before the socket is bound");
    assert.equal(seen.urlsAtConfigure, null, "resolvedUrls is null until listen (Vite: set on listen)");
    assert.equal(seen.listening, true, 'httpServer.once("listening") fires after configureServer');
    assert.deepEqual(
      seen.addressAtListening,
      { address: "127.0.0.1", family: "IPv4", port: 6402 },
      "httpServer.address() is the dev port once listening",
    );
    assert.deepEqual(
      seen.urlsAtListening,
      { local: ["http://localhost:6402/"], network: [], networkInterfaceNames: [] },
      "resolvedUrls is resolved before listening callbacks run (Vite prepends its handler)",
    );
    assert.equal(seen.unknownIsUndefined, true, "getModuleById(unknown virtual id) is undefined like Vite");
    assert.equal(seen.unknownFileIsUndefined, true, "getModuleById(missing file) is undefined");
    assert.equal(seen.realFileIsNode, true, "a real file oj may serve gets a node");
    assert.equal(seen.fileMapPopulated, true, "fileToModulesMap tracks minted nodes");
    assert.equal(seen.unknownFileSet, true, "getModulesByFile(unknown) is undefined like Vite");
  } finally {
    host.close();
    fx.cleanup();
  }
});
