// SPDX-License-Identifier: MIT
//
// Vite's resolveConfig runs resolveEnvironmentResolveOptions for every
// environment, so `environments[name].resolve` is always a complete object
// (external:[], noExternal:[], dedupe:[], ...). Plugins read it in
// configResolved -- @cloudflare/vite-plugin's validateWorkerEnvironmentOptions
// inspects `environments[worker].resolve.external` and crashes when resolve is
// undefined. The SSR plugin bridge must give every environment a resolve.
import { test } from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { repo } from "./harness.mjs";

const bridge = await import(pathToFileURL(join(repo, "crates/oj_server/src/assets/start/vite-plugin-bridge.mjs")).href);

test("configResolved sees a complete environments[env].resolve (cloudflare plugin's .external read does not crash)", async () => {
  let seen = null;
  const plugins = [
    {
      // Mirror @cloudflare/vite-plugin's configResolved: read the worker
      // environment's resolve.external. A bare `{}` environment (no resolve)
      // makes this throw "Cannot read properties of undefined (reading 'external')".
      name: "vite-plugin-cloudflare:config",
      configResolved(config) {
        const r = config.environments.ssr.resolve;
        seen = { external: r.external, noExternal: r.noExternal, isArray: Array.isArray(r.external) };
      },
      transform() {
        return null;
      },
    },
  ];
  const container = bridge.createPluginContainer({}, plugins, {
    command: "serve",
    environment: "ssr",
    // The environments arrive as bare objects (as an extracted config or the
    // bridge's own default does); the bridge must still populate their resolve.
    config: { root: repo, environments: { client: {}, ssr: {} } },
  });
  // resolveId triggers the lazy configResolved dispatch.
  await container.resolveId("virtual:probe", undefined);

  assert.ok(seen, "configResolved ran without throwing on an undefined resolve");
  assert.deepEqual(seen.external, [], "environments.ssr.resolve.external defaults to []");
  assert.deepEqual(seen.noExternal, [], "environments.ssr.resolve.noExternal defaults to []");
  assert.equal(seen.isArray, true);
});
