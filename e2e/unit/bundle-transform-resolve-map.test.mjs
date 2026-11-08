// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { repo, rpcSidecar, tmpProject } from "./harness.mjs";

// A Start bundle host, as start_bundle/hosts.rs spawns one: the bundle's own
// resolution answers `this.resolve` through the `bundlerResolve` reverse RPC.
function bundleHost(fx) {
  return rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        config: { root: fx.root, base: "/", mode: "production", command: "build" },
        env: { command: "build", mode: "production" },
        environment: { name: "client", mode: "build" },
        ojStartMode: true,
        ojBundleHost: true,
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
}

// The fixture plugin resolves one existing import per transform the way
// import-protection does (`this.resolve(src, id)`), plus a miss and a
// resolve for a different importer.
const pluginsSource = `export default [
  {
    name: "probe",
    async transform(code, id) {
      if (!id.endsWith("main.js")) return null;
      const hit = await this.resolve("./a.js", id);
      const miss = await this.resolve("./missing.js", id);
      const other = await this.resolve("./a.js", id + "?other-importer");
      return code + "\\n/*" + JSON.stringify({
        hit: hit && hit.id,
        miss,
        other: other && other.id,
      }) + "*/";
    },
  },
];\n`;

// Drive one transform to completion, answering `bundlerResolve` like the Rust
// side does (the bundler's own resolution), and record every round-trip.
async function drive(host, msg, aAbs) {
  const rpcs = [];
  let frame = await host.send(msg);
  while (frame.rpc != null) {
    rpcs.push(frame);
    let result = null;
    if (frame.method === "bundlerResolve" && frame.args[0] === "./a.js") {
      result = { id: aAbs };
    }
    host.child.stdin.write(JSON.stringify({ rpcReply: frame.rpc, result }) + "\n");
    frame = await host.nextFrame();
  }
  return { frame, rpcs };
}

function fixture() {
  const fx = tmpProject({ prefix: "oj-bundlemap-" });
  fx.write("a.js", "export const A = 1;\n");
  fx.write("main.js", "import './a.js';\nimport './missing.js';\n");
  fx.write("oj.plugins.mjs", pluginsSource);
  return fx;
}

function probeOf(frame, host, what) {
  assert.equal(frame.error, undefined, `${what} must not fail: ${frame.error}; stderr:\n${host.stderr()}`);
  const out = JSON.parse(frame.result);
  return JSON.parse(out.code.match(/\/\*(.*)\*\//s)[1]);
}

test("bundle transform this.resolve answers a mapped import without a bundlerResolve round-trip", async () => {
  const fx = fixture();
  const host = bundleHost(fx);
  try {
    const mainAbs = path.join(fx.root, "main.js");
    const aAbs = path.join(fx.root, "a.js");
    const code = "import './a.js';\nimport './missing.js';\n";
    const map = JSON.stringify({ "./a.js": aAbs });

    const mapped = await drive(host, { id: 1, hook: "transform", args: [code, mainAbs, map] }, aAbs);
    const mappedProbe = probeOf(mapped.frame, host, "mapped transform");
    assert.equal(mappedProbe.hit, aAbs, "the mapped import resolves to the map value");
    assert.equal(mappedProbe.miss, null, "an unmapped import still resolves null");
    assert.equal(mappedProbe.other, aAbs, "a different importer resolves through the bundler");
    const mappedRpcs = mapped.rpcs.filter((r) => r.method === "bundlerResolve").map((r) => r.args);
    assert.deepEqual(
      mappedRpcs,
      [
        ["./missing.js", mainAbs, false],
        ["./a.js", `${mainAbs}?other-importer`, false],
      ],
      "only misses and foreign importers round-trip; the mapped import never leaves the host",
    );

    const bare = await drive(host, { id: 2, hook: "transform", args: [code, mainAbs, "{}"] }, aAbs);
    const bareProbe = probeOf(bare.frame, host, "fallback transform");
    assert.deepEqual(bareProbe, mappedProbe, "the fallback path returns the identical result");
    const bareRpcs = bare.rpcs.filter((r) => r.method === "bundlerResolve").map((r) => r.args[0]);
    assert.deepEqual(
      bareRpcs,
      ["./a.js", "./missing.js", "./a.js"],
      "without the map every import round-trips through the bundler",
    );
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("bundle transformPhase takes the same pre-resolved map", async () => {
  const fx = fixture();
  const host = bundleHost(fx);
  try {
    const mainAbs = path.join(fx.root, "main.js");
    const aAbs = path.join(fx.root, "a.js");
    const code = "import './a.js';\nimport './missing.js';\n";
    const map = JSON.stringify({ "./a.js": aAbs });

    // The probe has no `enforce`, so it runs in the after-compile slot —
    // where import-protection runs in Start bundles — not before-compile.
    const { frame, rpcs } = await drive(
      host,
      { id: 1, hook: "transformPhase", args: [code, mainAbs, map, "after-compile"] },
      aAbs,
    );
    const probe = probeOf(frame, host, "phased transform");
    assert.equal(probe.hit, aAbs, "the phased transform sees the same map");
    assert.ok(
      !rpcs.some((r) => r.method === "bundlerResolve" && r.args[0] === "./a.js" && r.args[1] === mainAbs),
      "the mapped import does not round-trip in a phase either",
    );
  } finally {
    host.close();
    fx.cleanup();
  }
});
