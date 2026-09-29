// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

// Vite hands the `config` hook the user config (whose `plugins` is the flat
// plugin array) and `configResolved` a fully-defaulted config. Plugins such as
// @crxjs read `config.plugins`, and UnoCSS resolves `config.build.outDir` /
// reads `config.build.rollupOptions.output`. oj must supply both.
test("config and configResolved expose config.plugins and build defaults", async () => {
  const fx = tmpProject({ prefix: "oj-cfg-" });
  fx.write(
    "oj.plugins.mjs",
    `let seen = {};
     export default [
       {
         name: "reader",
         config(config) {
           seen.configPlugins = Array.isArray(config.plugins) ? config.plugins.length : -1;
         },
         configResolved(config) {
           seen.outDir = config.build.outDir;
           seen.assetsDir = config.build.assetsDir;
           seen.hasRollupOptions = config.build.rollupOptions !== undefined;
           seen.resolvedPlugins = Array.isArray(config.plugins) ? config.plugins.length : -1;
           seen.names = (config.plugins || []).map((p) => p && p.name);
           seen.publicDir = config.publicDir;
         },
         transform(code, id) {
           if (id.endsWith("probe.js")) return "export default " + JSON.stringify(seen) + ";";
           return null;
         },
       },
       { name: "sibling-a" },
       { name: "sibling-b" },
       // A serve-only plugin must not appear in the build's config.plugins.
       { name: "dev-only", apply: "serve" },
       // A config hook returning a partial with its own plugins must not
       // accumulate into config.plugins across the config-hook merge.
       { name: "adds-config", config() { return { define: { X: "1" } }; } },
     ];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        config: { root: fx.root },
        env: { command: "build", mode: "production" },
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
    // The build-applicable plugins are visible (oj also injects a vite:css-post shim).
    assert.ok(seen.configPlugins >= 3, "the config hook sees the flat plugin array");
    assert.ok(seen.resolvedPlugins >= 3, "configResolved sees the plugin array too");
    assert.equal(seen.outDir, "dist", "build.outDir defaults to dist");
    assert.equal(seen.assetsDir, "assets", "build.assetsDir defaults to assets");
    assert.equal(seen.hasRollupOptions, true, "build.rollupOptions is present");
    // publicDir defaults to an absolute <root>/public (Vite parity); plugins
    // like @crxjs read it to locate manifest assets.
    assert.equal(seen.publicDir, path.join(fx.root, "public"), "publicDir defaults to absolute <root>/public");
    // A serve-only plugin is excluded from the build's config.plugins.
    assert.ok(!seen.names.includes("dev-only"), "serve-only plugin excluded from build config.plugins");
    // The vite:css-post shim is present.
    assert.ok(seen.names.includes("vite:css-post"), "vite:css-post shim injected");
    // No duplicates: config.plugins is pinned across config-hook merges.
    const dupes = seen.names.filter((n, i) => n && seen.names.indexOf(n) !== i);
    assert.deepEqual(dupes, [], "config.plugins has no duplicate entries");
  } finally {
    host.close();
    fx.cleanup();
  }
});
