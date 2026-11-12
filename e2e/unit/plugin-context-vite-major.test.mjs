// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

// The plugin context's shape follows the app's Vite major. Vite 6 added the
// Environment API (`this.environment` on the plugin context, the `hotUpdate`,
// `configEnvironment` and `applyToEnvironment` hooks); Vite 5's PluginContext
// is `meta`, `ssr` and the rollup members, and its handleHMRUpdate runs
// `handleHotUpdate` alone. A plugin that supports both lines picks its path by
// `this.environment` (`@tailwindcss/vite`: present, it calls `createIdResolver`
// off the app's `vite`, which Vite 5 does not export), so a Vite 5 app's
// plugins must see the Vite 5 shape.

// A plugin with every hook a two-line plugin declares, each reporting what
// it saw through the transform's output.
const pluginsSource = `const seen = { hotUpdate: 0, handleHotUpdate: 0, configEnvironment: 0, applyToEnvironment: 0 };
export default [{
  name: "probe",
  configEnvironment() { seen.configEnvironment++; },
  applyToEnvironment() { seen.applyToEnvironment++; return true; },
  hotUpdate() { seen.hotUpdate++; return []; },
  handleHotUpdate() { seen.handleHotUpdate++; return []; },
  transform(code, id) {
    if (!id.endsWith("probe.js")) return null;
    return "/*" + JSON.stringify({
      ...seen,
      hasEnvironment: "environment" in this,
      environment: this.environment === undefined ? null : this.environment.name,
      viteVersion: this.meta.viteVersion,
    }) + "*/";
  },
}];\n`;

// A project whose \`node_modules/vite\` reports \`version\` (what the host
// reads off the app's Vite), or none at all.
function project(version) {
  const fx = tmpProject({ prefix: "oj-plugin-ctx-", pkgJson: { name: "app", type: "module" } });
  if (version) {
    fx.write(
      "node_modules/vite/package.json",
      JSON.stringify({ name: "vite", version, type: "module", main: "index.js" }),
    );
    fx.write("node_modules/vite/index.js", `export const version = ${JSON.stringify(version)};\n`);
  }
  fx.write("probe.js", "export const x = 1;\n");
  fx.write("oj.plugins.mjs", pluginsSource);
  return fx;
}

async function probe(version) {
  const fx = project(version);
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const file = path.join(fx.root, "probe.js");
    const hot = await host.send({ id: 1, hook: "handleHotUpdate", args: [file, "123", "update", "[]"] });
    assert.equal(hot.id, 1, `hot update reply expected, got ${JSON.stringify(hot)}; stderr:\n${host.stderr()}`);
    const frame = await host.send({ id: 2, hook: "transform", args: ["export const x = 1;\n", file, ""] });
    assert.equal(frame.id, 2, `transform reply expected, got ${JSON.stringify(frame)}; stderr:\n${host.stderr()}`);
    assert.equal(frame.error, undefined, `transform must not fail: ${frame.error}`);
    const out = JSON.parse(frame.result);
    return JSON.parse(out.code.match(/\/\*(\{.*\})\*\//s)[1]);
  } finally {
    host.close();
    fx.cleanup();
  }
}

test("a Vite 5 app's plugins get Vite 5's context and hooks: no environment, handleHotUpdate only", async () => {
  const seen = await probe("5.4.21");
  assert.equal(seen.viteVersion, "5.4.21", "the meta reports the app's Vite");
  assert.equal(seen.hasEnvironment, false, `Vite 5's PluginContext has no environment member: ${JSON.stringify(seen)}`);
  assert.equal(seen.environment, null);
  assert.equal(seen.handleHotUpdate, 1, "Vite 5's handleHMRUpdate runs handleHotUpdate");
  assert.equal(seen.hotUpdate, 0, "and knows no hotUpdate");
  assert.equal(seen.configEnvironment, 0, "Vite 5's resolveConfig has no configEnvironment step");
  assert.equal(seen.applyToEnvironment, 0, "and resolves no plugins per environment");
});

test("a Vite 6 app's plugins, and an app without Vite, get the Environment API shape", async () => {
  for (const version of ["6.3.5", null]) {
    const seen = await probe(version);
    assert.equal(seen.hasEnvironment, true, `${version ?? "no vite"}: ${JSON.stringify(seen)}`);
    assert.equal(seen.environment, "client");
    assert.equal(seen.hotUpdate, 1, "hotUpdate runs, with this.environment");
    assert.equal(seen.handleHotUpdate, 0, "in place of the legacy hook");
    assert.equal(seen.configEnvironment > 0, true, "configEnvironment ran for the environments");
    assert.equal(seen.applyToEnvironment > 0, true, "applyToEnvironment ran");
  }
});
