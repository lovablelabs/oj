// SPDX-License-Identifier: MIT
// Vite's `vite:alias` step runs when no resolveId hook claims an id: the merged
// config's `resolve.alias` (including entries from plugin `config` hooks, e.g.
// vite-plugin-solid mapping /^solid-refresh$/ to /@solid-refresh) rewrites it.
// Plugin claims still win over aliases. Regression: solid-refresh resolved to
// null (then a 500 "Failed to resolve import") because the chain had no alias
// step at all.
import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

const pluginsSource = `export default [
  {
    name: "solid-like",
    config() {
      return { resolve: { alias: [{ find: /^solid-refresh$/, replacement: "/@solid-refresh" }] } };
    },
    resolveId(id) { return id === "/@solid-refresh" ? id : null; },
    load(id) { return id === "/@solid-refresh" ? "export const runtime = true;" : null; },
  },
  {
    name: "claimer",
    resolveId(id) { return id === "claimed" ? "\\\\0claimed" : null; },
  },
];\n`;

function hostFor(fx) {
  return rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
}

async function drive(host, msg) {
  let frame = await host.send(msg);
  while (frame.rpc != null) {
    host.child.stdin.write(JSON.stringify({ rpcReply: frame.rpc, result: null }) + "\n");
    frame = await host.nextFrame();
  }
  return frame;
}

test("resolveId falls back to config-hook regex aliases; plugin claims win", async () => {
  const fx = tmpProject({ prefix: "oj-aliasfb-" });
  fx.write("oj.plugins.mjs", pluginsSource);
  const host = hostFor(fx);
  try {
    // Run the config hooks so resolvedConfig carries the alias.
    await drive(host, { id: 1, hook: "getPluginConfig", args: [] });
    const aliased = await drive(host, { id: 2, hook: "resolveId", args: ["solid-refresh", "main.js"] });
    assert.equal(aliased.result, "/@solid-refresh", "regex alias rewrites the bare runtime id");
    const claimed = await drive(host, { id: 3, hook: "resolveId", args: ["claimed", "main.js"] });
    assert.equal(claimed.result, "\\0claimed", "a plugin claim still wins");
    const miss = await drive(host, { id: 4, hook: "resolveId", args: ["nothing-here", "main.js"] });
    assert.equal(miss.result, null, "unmatched ids still resolve to null");
  } finally {
    host.close();
    fx.cleanup();
  }
});
