// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

// Vite's ctx.resolve runs the container's resolveId chain (skipping the caller
// unless skipSelf: false) before its own resolver, and ctx.load runs the plugins'
// load + transform chain before reading the file. The host used to go straight
// to the Rust resolver and the disk, so a sibling plugin's virtual id came back
// null / threw.
const pluginsSource = `export default [
  {
    name: "virt",
    resolveId(id) { return id === "virtual:msg" ? "\\0virtual:msg" : null; },
    load(id) { return id === "\\0virtual:msg" ? "export const MSG = 'from-virtual';" : null; },
    transform(code, id) { return id === "\\0virtual:msg" ? code + "\\n// virt-transformed" : null; },
  },
  {
    name: "consumer",
    resolveId(id) { return id === "self:x" ? "\\0self" : null; },
    async transform(code, id) {
      if (!id.endsWith("main.js")) return null;
      const r = await this.resolve("virtual:msg", id);
      const info = await this.load({ id: r.id });
      const selfSkipped = await this.resolve("self:x", id);
      const selfKept = await this.resolve("self:x", id, { skipSelf: false });
      const disk = await this.resolve("./a.js", id);
      return code + "\\n/*" + JSON.stringify({
        resolved: r.id,
        loaded: info.code,
        selfSkipped,
        selfKept: selfKept && selfKept.id,
        disk: disk && disk.id,
      }) + "*/";
    },
  },
];\n`;

// Drive one hook to completion, answering the host's ctx RPCs like the Rust
// side would: the disk resolver knows ./a.js and nothing else.
async function drive(host, fx, msg) {
  const rpcs = [];
  let frame = await host.send(msg);
  while (frame.rpc != null) {
    rpcs.push(frame);
    let result = null;
    if (frame.method === "resolve" && frame.args[0] === "./a.js") result = path.join(fx.root, "a.js");
    host.child.stdin.write(JSON.stringify({ rpcReply: frame.rpc, result }) + "\n");
    frame = await host.nextFrame();
  }
  return { frame, rpcs };
}

test("ctx.resolve and ctx.load consult sibling plugins before the Rust resolver and disk", async () => {
  const fx = tmpProject({ prefix: "oj-ctxchain-" });
  fx.write("a.js", "export const A = 1;\n");
  fx.write("main.js", "import './a.js';\n");
  fx.write("oj.plugins.mjs", pluginsSource);
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const { frame, rpcs } = await drive(host, fx, {
      id: 1,
      hook: "transform",
      args: ["import './a.js';\n", path.join(fx.root, "main.js"), ""],
    });
    assert.equal(frame.id, 1, `transform reply expected, got ${JSON.stringify(frame)}; stderr:\n${host.stderr()}`);
    assert.equal(frame.error, undefined, `transform must not fail: ${frame.error}`);
    const out = JSON.parse(frame.result);
    const probe = JSON.parse(out.code.match(/\/\*(\{.*\})\*\//s)[1]);
    assert.equal(probe.resolved, "\0virtual:msg", "this.resolve hits the sibling's resolveId first");
    assert.equal(
      probe.loaded,
      "export const MSG = 'from-virtual';\n// virt-transformed",
      "this.load runs the sibling's load and the transform chain",
    );
    assert.equal(probe.selfSkipped, null, "the calling plugin's own resolveId is skipped (Vite skipSelf default)");
    assert.equal(probe.selfKept, "\0self", "skipSelf: false lets the caller resolve its own id");
    assert.equal(probe.disk, path.join(fx.root, "a.js"), "ids no plugin claims still reach the Rust resolver");
    const resolveRpcs = rpcs.filter((r) => r.method === "resolve").map((r) => r.args[0]);
    assert.ok(!resolveRpcs.includes("virtual:msg"), "a plugin-resolved id never round-trips to Rust");
    assert.ok(!rpcs.some((r) => r.method === "moduleInfo"), "a plugin-loaded id never hits the disk reader");
    assert.ok(resolveRpcs.includes("./a.js"), "unclaimed ids fall through to the Rust resolver");
  } finally {
    host.close();
    fx.cleanup();
  }
});
