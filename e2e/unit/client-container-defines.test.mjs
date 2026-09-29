// SPDX-License-Identifier: MIT

// The Start dev client bundle applies the config's `define` map the way the
// SSR loader does (Vite's define plugin runs in every environment): top-level
// `define`, then `environments.client.define`, plus whatever a plugin's
// `config` hook merged in, serialized like handleDefineValue (strings verbatim,
// anything else JSON). Without it a define a component references renders on
// the server and throws a ReferenceError on hydration.

import { test } from "node:test";
import assert from "node:assert/strict";
import { existsSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { repo } from "./harness.mjs";

const fixture = join(repo, "e2e/fixtures/start-app");
const installed = existsSync(join(fixture, "node_modules/vite"));

const app = mkdtempSync(join(tmpdir(), "oj-client-defines-"));
writeFileSync(join(app, "package.json"), JSON.stringify({ name: "app", type: "module" }));
writeFileSync(
  join(app, "vite.config.mjs"),
  [
    "export default {",
    '  define: { __TOP__: JSON.stringify("top"), __OVERRIDE__: "false", __NUMBER__: 42, __OBJ__: { a: 1 } },',
    "  environments: {",
    '    client: { define: { __CLIENT_ONLY__: "true", __OVERRIDE__: "true" } },',
    '    ssr: { define: { __SERVER_ONLY__: "true" } },',
    "  },",
    '  plugins: [{ name: "adds-define", config() { return { define: { __FROM_HOOK__: JSON.stringify("hook") } }; } }],',
    "};",
  ].join("\n"),
);
if (installed) symlinkSync(join(fixture, "node_modules"), join(app, "node_modules"), "dir");
process.on("exit", () => rmSync(app, { recursive: true, force: true }));

const maybe = installed ? test : test.skip;
const bridge = installed
  ? await import(pathToFileURL(join(repo, "crates/oj_server/src/assets/start/vite-plugin-bridge.mjs")).href)
  : null;

maybe("the client container exposes the config's define map for its environment", async () => {
  const container = await bridge.loadPluginContainer(app, { command: "serve", environment: "client" });
  assert.ok(container, "config loaded");
  const defines = container.defines();
  assert.equal(defines.__TOP__, '"top"');
  assert.equal(defines.__CLIENT_ONLY__, "true");
  assert.equal(defines.__OVERRIDE__, "true", "environments.client.define wins over the top-level define");
  assert.equal(defines.__NUMBER__, "42", "non-string values are JSON-serialized");
  assert.equal(defines.__OBJ__, '{"a":1}');
  assert.equal(defines.__FROM_HOOK__, '"hook"', "a plugin config() hook's define is merged in");
  assert.equal(defines.__SERVER_ONLY__, undefined, "the ssr environment's define stays server-side");
});

maybe("the ssr container sees the ssr environment's define instead", async () => {
  const container = await bridge.loadPluginContainer(app, { command: "serve", environment: "ssr" });
  const defines = container.defines();
  assert.equal(defines.__SERVER_ONLY__, "true");
  assert.equal(defines.__OVERRIDE__, "false");
  assert.equal(defines.__CLIENT_ONLY__, undefined);
});
