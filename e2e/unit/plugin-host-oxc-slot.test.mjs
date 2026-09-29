// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { rpcSidecar, tmpProject } from "./harness.mjs";

// Vite's plugin order puts `vite:oxc` (the TS/JSX strip) after the user's
// enforce:"pre" plugins and before the normal and post ones
// (plugins/index.ts resolvePlugins), so only a pre plugin, or a hook with
// `order: "pre"`, sees TypeScript/JSX; every later transform gets JS and can
// `this.parse` it. oj used to hand the whole chain the raw source, so an
// AST-reading post plugin failed to parse every .ts/.tsx module.

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const viteSrc = path.join(repo, "e2e/fixtures/start-app/node_modules/vite");
const haveVite = fs.existsSync(path.join(viteSrc, "package.json"));
const viteTest = haveVite ? test : (name, fn) => test(name, { skip: "fixture vite not installed" }, () => {});

const PLUGINS = `const seen = {};
const record = (key) => function (code, id) {
  if (!id.endsWith(".tsx")) return null;
  let parses = true;
  try { this.parse(code); } catch { parses = false; }
  seen[key] = { hasTs: code.includes(": number"), hasJsx: code.includes("<div>"), parses };
  return null;
};
export default [
  { name: "pre", enforce: "pre", transform: record("pre") },
  { name: "order-pre", transform: { order: "pre", handler: record("orderPre") } },
  {
    name: "normal",
    transform(code, id) {
      if (id.endsWith("probe.js")) return JSON.stringify(seen);
      return record("normal").call(this, code, id);
    },
  },
  { name: "post", enforce: "post", transform: record("post") },
];
`;

const SOURCE = "const n: number = 1;\nexport const A = () => <div>{n}</div>;\n";

async function observe(extraConfig) {
  const fx = tmpProject({ prefix: "oj-oxc-slot-" });
  fs.symlinkSync(viteSrc, path.join(fx.root, "node_modules", "vite"), "dir");
  fx.write("oj.plugins.mjs", PLUGINS);
  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        config: { root: fx.root, ...extraConfig },
        env: { command: "serve", mode: "development" },
        environment: { name: "ssr", mode: "dev" },
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const res = await host.send({ id: 1, hook: "transform", args: [SOURCE, path.join(fx.root, "src/a.tsx"), ""] });
    assert.equal(res.error, undefined, `transform must not throw: ${res.error}\n${host.stderr()}`);
    const probe = await host.send({ id: 2, hook: "transform", args: ["", path.join(fx.root, "probe.js"), ""] });
    return JSON.parse(JSON.parse(probe.result).code);
  } finally {
    host.close();
    fx.cleanup();
  }
}

viteTest("TS/JSX is stripped at vite:oxc's slot: pre hooks see it, normal and post parse JS", async () => {
  const seen = await observe({});
  assert.deepEqual(seen.pre, { hasTs: true, hasJsx: true, parses: false }, "an enforce:pre plugin sees the raw source");
  assert.deepEqual(
    seen.orderPre,
    { hasTs: true, hasJsx: true, parses: false },
    "an order:pre hook runs before the strip",
  );
  assert.deepEqual(seen.normal, { hasTs: false, hasJsx: false, parses: true }, "a normal plugin gets JS");
  assert.deepEqual(seen.post, { hasTs: false, hasJsx: false, parses: true }, "a post plugin gets JS it can this.parse");
});

viteTest("oxc: false keeps the raw source for every plugin, as in Vite", async () => {
  const seen = await observe({ oxc: false });
  assert.equal(seen.post.hasTs, true, "with oxc disabled Vite runs no strip");
  assert.equal(seen.post.parses, false);
});

viteTest("oxc.exclude keeps the excluded module raw for normal/post plugins", async () => {
  // Vite's vite:oxc filter is createFilter(include || ts-default, exclude ||
  // /\.js$/): an excluded module reaches every later plugin untransformed.
  // (a glob string: the config crosses the RPC as JSON, where a RegExp cannot)
  const seen = await observe({ oxc: { exclude: ["**/src/a.tsx"] } });
  assert.equal(seen.post.hasTs, true, "the excluded module is not stripped");
  assert.equal(seen.post.parses, false);
});

viteTest("a filter.code matching only compiled output still claims the module", async () => {
  // Vite evaluates a hook's filter against the code the hook receives, which
  // for a normal/post plugin is the post-oxc output: a code filter matching
  // the automatic-runtime import must fire although the raw source never
  // contains it (dev emits react/jsx-dev-runtime, prod react/jsx-runtime).
  const fx = tmpProject({ prefix: "oj-oxc-codefilter-" });
  fs.symlinkSync(viteSrc, path.join(fx.root, "node_modules", "vite"), "dir");
  fx.write(
    "oj.plugins.mjs",
    `const seen = { claimed: false };
export default [
  {
    name: "jsx-runtime-probe",
    enforce: "post",
    transform: {
      filter: { code: "react/jsx" }, // matches jsx-runtime AND jsx-dev-runtime, never the raw source
      handler(code, id) {
        if (id.endsWith(".tsx")) seen.claimed = true;
        return null;
      },
    },
  },
  { name: "probe", transform(code, id) { return id.endsWith("probe.js") ? JSON.stringify(seen) : null; } },
];
`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        config: { root: fx.root },
        env: { command: "serve", mode: "development" },
        environment: { name: "ssr", mode: "dev" },
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const res = await host.send({ id: 1, hook: "transform", args: [SOURCE, path.join(fx.root, "src/a.tsx"), ""] });
    assert.equal(res.error, undefined, `transform must not throw: ${res.error}\n${host.stderr()}`);
    const probe = await host.send({ id: 2, hook: "transform", args: ["", path.join(fx.root, "probe.js"), ""] });
    const seen = JSON.parse(JSON.parse(probe.result).code);
    assert.equal(seen.claimed, true, "the post plugin's code filter fires on the stripped output");
  } finally {
    host.close();
    fx.cleanup();
  }
});
