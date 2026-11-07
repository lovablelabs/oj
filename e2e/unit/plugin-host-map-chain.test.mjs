// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

// A bundle takes one map per module from oj's host, so the host reports the
// chain: each plugin's map in order, and the first plugin that returned code
// without one. Rollup counts that as a missing map (the chain breaks);
// `map: null` says the edit needs none.

const MAP = `{ version: 3, sources: ["x.js"], names: [], mappings: "AAAA" }`;

const PLUGINS = `export default [{
  name: "string",
  transform(code, id) { return id.endsWith("/string.js") ? code + ";" : null; },
  renderChunk(code) { return code + ";"; },
}, {
  name: "null-map",
  transform(code, id) { return id.endsWith("/null.js") ? { code: code + ";", map: null } : null; },
}, {
  name: "mapped",
  transform(code, id) { return id.endsWith("/mapped.js") ? { code: code + ";", map: ${MAP} } : null; },
}, {
  name: "no-map",
  transform(code, id) { return id.endsWith("/nomap.js") ? { code: code + ";" } : null; },
}, {
  transform(code, id) { return id.endsWith("/unnamed.js") ? code + ";" : null; },
}];
`;

test("the host reports each plugin's map and the first that left none", async () => {
  const fx = tmpProject({ prefix: "oj-map-chain-" });
  fx.write("oj.plugins.mjs", PLUGINS);
  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        config: { root: fx.root },
        env: { command: "build", mode: "production" },
        environment: { name: "client", mode: "build" },
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  let id = 0;
  const call = async (hook, args) => {
    const res = await host.send({ id: ++id, hook, args });
    assert.equal(res.error, undefined, `${hook} must not throw: ${res.error}\n${host.stderr()}`);
    return JSON.parse(res.result);
  };
  const chain = async (name) => {
    const r = await call("transform", ["export const a = 1", path.join(fx.root, name), ""]);
    return { maps: r.maps.length, mapPlugins: r.mapPlugins, mapBroken: r.mapBroken };
  };
  try {
    assert.deepEqual(await chain("string.js"), { maps: 0, mapPlugins: [], mapBroken: "string" });
    assert.deepEqual(await chain("null.js"), { maps: 0, mapPlugins: [], mapBroken: null });
    assert.deepEqual(await chain("mapped.js"), { maps: 1, mapPlugins: ["mapped"], mapBroken: null });
    assert.deepEqual(await chain("nomap.js"), { maps: 0, mapPlugins: [], mapBroken: "no-map" });
    // A plugin without a name still breaks the chain.
    assert.deepEqual(await chain("unnamed.js"), { maps: 0, mapPlugins: [], mapBroken: "?" });
    const rendered = await call("renderChunk", ["export {}", "{}"]);
    assert.equal(rendered.code, "export {};");
    assert.equal(rendered.mapBroken, "string");
  } finally {
    host.close();
    fx.cleanup();
  }
});
