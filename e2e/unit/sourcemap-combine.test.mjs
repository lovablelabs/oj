// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Rollup's `this.getCombinedSourcemap()` in oj's plugin host: the maps of a
// module's earlier transforms collapsed back to the loaded code (TanStack
// Start's import protection points its code frames through it).

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { repo, rpcSidecar, tmpProject } from "./harness.mjs";

const { combineSourcemaps, decodeMappings, encodeMappings } = await import(
  pathToFileURL(path.join(repo, "crates/oj_server/src/assets/sourcemap-combine.mjs")).href
);

// The original [line, column] the combined map gives generated (line, column).
function original(map, line, column) {
  const segs = decodeMappings(map.mappings)[line] ?? [];
  const seg = segs.filter((s) => s[0] <= column).at(-1);
  return seg && seg.length > 1 ? [seg[2], seg[3]] : null;
}

test("mappings decode and encode as VLQ", () => {
  for (const m of ["AAAA;AACA,EAAE", "AAAAA,EAAE;;ACAC", ""]) assert.equal(encodeMappings(decodeMappings(m)), m);
  assert.deepEqual(decodeMappings("AAAA,CAAC;AACA"), [
    [
      [0, 0, 0, 0],
      [1, 0, 0, 1],
    ],
    [[0, 0, 1, 1]],
  ]);
});

test("with no earlier map it is rollup's hires identity map of the loaded code", () => {
  const map = combineSourcemaps("/a.js", "ab\ncd", []);
  assert.deepEqual(map.sources, ["/a.js"]);
  assert.deepEqual(map.sourcesContent, ["ab\ncd"]);
  assert.deepEqual(original(map, 1, 1), [1, 1]);
  assert.deepEqual(original(map, 0, 0), [0, 0]);
});

test("a chain a transform broke maps nothing", () => {
  assert.equal(combineSourcemaps("/a.js", "ab", [], true).mappings, "");
});

test("a chain collapses back to the loaded code, names kept", () => {
  // The first transform prefixes "X" (generated column c+1 is original c);
  // the second adds a line on top (generated line l+1 is its input's l).
  const first = {
    version: 3,
    sources: ["/a.js"],
    names: ["foo"],
    mappings: encodeMappings([
      [
        [1, 0, 0, 0, 0],
        [5, 0, 0, 4],
      ],
    ]),
  };
  const second = {
    version: 3,
    sources: ["/a.js"],
    names: [],
    mappings: encodeMappings([
      [],
      [
        [0, 0, 0, 0],
        [1, 0, 0, 1],
        [5, 0, 0, 5],
      ],
    ]),
  };
  const map = combineSourcemaps("/a.js", "const a = 1;", [JSON.stringify(first), second]);
  assert.equal(original(map, 0, 0), null, "the added line maps nowhere");
  assert.deepEqual(original(map, 1, 5), [0, 4]);
  assert.deepEqual(original(map, 1, 1), [0, 0]);
  assert.deepEqual(original(map, 1, 0), null, "the prefix maps nowhere");
  const [seg] = decodeMappings(map.mappings)[1].filter((s) => s[0] === 1);
  assert.equal(map.names[seg[4]], "foo");
});

test("a transform hook's getCombinedSourcemap covers the transforms before it", async () => {
  const fx = tmpProject({ prefix: "oj-combined-map-" });
  const shift = JSON.stringify({
    version: 3,
    sources: ["x.js"],
    names: [],
    mappings: encodeMappings([[[2, 0, 0, 0]]]),
  });
  fx.write(
    "oj.plugins.mjs",
    `export default [{
  name: "prefix",
  transform(code) { return { code: "  " + code, map: ${shift} }; },
}, {
  name: "reader",
  transform(code, id) {
    const map = this.getCombinedSourcemap();
    return { code: code + "\\n//" + JSON.stringify(map), map: null };
  },
}];
`,
  );
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
  try {
    const id = path.join(fx.root, "x.js");
    const res = await host.send({ id: 1, hook: "transform", args: ["export const a = 1", id, ""] });
    assert.equal(res.error, undefined, `${res.error}\n${host.stderr()}`);
    const code = JSON.parse(res.result).code;
    const map = JSON.parse(code.slice(code.indexOf("//") + 2));
    assert.deepEqual(map.sources, [id]);
    assert.deepEqual(map.sourcesContent, ["export const a = 1"]);
    assert.deepEqual(original(map, 0, 2), [0, 0], "the reader's column 2 is the source's column 0");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("a segment on another source maps nowhere, as each link has one source", () => {
  const twoSources = {
    version: 3,
    sources: ["/a.js", "/helper.js"],
    names: [],
    mappings: encodeMappings([
      [
        [0, 0, 0, 0],
        [4, 1, 0, 0],
      ],
    ]),
  };
  const map = combineSourcemaps("/a.js", "abcd efgh", [twoSources]);
  assert.deepEqual(original(map, 0, 0), [0, 0]);
  assert.equal(decodeMappings(map.mappings)[0].length, 1, "the helper.js segment is dropped");
});

// A map-less transform breaks a build's chain (Rollup's missing link) but not
// a dev server's (Vite's _getCombinedSourcemap skips it).
for (const [command, mode, broken] of [
  ["build", "production", true],
  ["serve", "development", false],
]) {
  test(`after a map-less transform, ${command}'s combined map ${broken ? "maps nothing" : "still maps"}`, async () => {
    const fx = tmpProject({ prefix: "oj-combined-broken-" });
    fx.write(
      "oj.plugins.mjs",
      `export default [{
  name: "bare",
  transform(code) { return code + "\\n"; },
}, {
  name: "reader",
  transform(code) {
    return { code: code + "//" + JSON.stringify(this.getCombinedSourcemap()), map: null };
  },
}];
`,
    );
    const host = rpcSidecar("plugin-host.mjs", {
      args: [
        path.join(fx.root, "oj.plugins.mjs"),
        JSON.stringify({ config: { root: fx.root }, env: { command, mode }, environment: { name: "client" } }),
      ],
      env: { OJ_CACHE_ROOT: fx.root },
      cwd: fx.root,
    });
    try {
      const res = await host.send({
        id: 1,
        hook: "transform",
        args: ["export const a = 1", path.join(fx.root, "x.js"), ""],
      });
      assert.equal(res.error, undefined, `${res.error}\n${host.stderr()}`);
      const code = JSON.parse(res.result).code;
      const map = JSON.parse(code.slice(code.indexOf("//") + 2));
      assert.equal(map.mappings === "", broken, JSON.stringify(map));
    } finally {
      host.close();
      fx.cleanup();
    }
  });
}
