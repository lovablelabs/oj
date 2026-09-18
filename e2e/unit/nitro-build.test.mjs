// SPDX-License-Identifier: MIT
//
// oj builds Nitro's environment with rolldown for Nitro's buildApp hook. The
// options must carry Vite's defaults and, ahead of Nitro's own plugins, oj's
// plugin bridge and transforms, as Vite runs the app's plugins for the nitro
// environment too.
import { test } from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { repo } from "./harness.mjs";

const { nitroRolldownOptions, nitroBuilder } = await import(
  pathToFileURL(join(repo, "crates/oj_server/src/assets/start/nitro-build.mjs")).href
);

const nitroPlugin = { name: "nitro:virtual" };
const envConfig = {
  build: { rolldownOptions: { input: "entry.mjs", plugins: [nitroPlugin, null], output: { dir: "out" } } },
  resolve: { conditions: ["node", "import"] },
  define: { "process.env.NODE_ENV": '"production"' },
};

test("nitroRolldownOptions puts oj's plugins ahead of Nitro's and drops empty entries", () => {
  const bridge = { name: "oj-vite-plugins" }, assets = { name: "oj-assets" };
  const options = nitroRolldownOptions(envConfig, {}, {}, [bridge, [assets, undefined]]);
  assert.deepEqual(options.plugins, [bridge, assets, nitroPlugin]);
  assert.equal(options.input, "entry.mjs");
  assert.equal(options.platform, "node");
  assert.deepEqual(options.resolve.conditionNames, ["node", "import"]);
  assert.equal(options.transform.define["process.env.NODE_ENV"], '"production"');
});

test("nitroRolldownOptions keeps Nitro's plugins alone without oj plugins", () => {
  assert.deepEqual(nitroRolldownOptions(envConfig, {}).plugins, [nitroPlugin]);
});

test("nitroBuilder resolves its plugins per build and marks the environment built", async () => {
  const seen = [];
  const config = { environments: { client: {}, ssr: {}, nitro: envConfig } };
  const builder = nitroBuilder({
    config,
    build: async (options) => { seen.push(options); return { output: [] }; },
    envDefine: { "import.meta.env.SSR": "true" },
    plugins: async (env) => [{ name: `oj-for-${env.name}` }],
  });
  assert.equal(builder.environments.client.isBuilt, true);
  assert.equal(builder.environments.ssr.isBuilt, true);
  assert.equal(builder.environments.nitro.isBuilt, false);
  await builder.build(builder.environments.nitro);
  assert.equal(builder.environments.nitro.isBuilt, true);
  assert.deepEqual(seen[0].plugins.map((p) => p.name), ["oj-for-nitro", "nitro:virtual"]);
  assert.equal(seen[0].transform.define["import.meta.env.SSR"], "true");
  await assert.rejects(() => builder.build(builder.environments.ssr), /oj builds itself/);
});
