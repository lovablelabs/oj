// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Module-info `code` is a byte-budgeted recency window (OJ_INFO_CODE_MB), not
// a process-lifetime copy of every transformed module: Vite dev's ModuleInfo
// carries no code at all (its proxy throws), and the one retained copy lives
// in the budgeted Rust cache. The window keeps the just-transformed module's
// code readable (the TanStack ingest shape reads a dependency's info right
// after transforming it) while old code is dropped; meta always survives.
import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

test("info.code is evicted by the byte window, meta survives, fresh code readable", async () => {
  const fx = tmpProject({ prefix: "oj-info-window-" });
  fx.write(
    "oj.plugins.mjs",
    `let seen = {};
     export default [{
       name: "window-probe",
       transform(code, id) {
         if (id.endsWith("probe.js")) {
           const infoA = this.getModuleInfo(seen.aId);
           const infoB = this.getModuleInfo(seen.bId);
           return "export default " + JSON.stringify({
             aCode: infoA ? typeof infoA.code : "missing",
             aCodeValue: infoA?.code ?? null,
             aMeta: infoA?.meta ?? null,
             bHasCode: typeof infoB?.code === "string" && infoB.code.length > 0,
           }) + ";";
         }
         return { meta: { tag: id.endsWith("a.js") ? "A" : "B" } };
       },
       moduleParsed(info) {
         // The freshly transformed module's code is always readable here.
         if (info.id.endsWith("a.js")) { seen.aId = info.id; seen.aFreshCode = typeof info.code === "string" && info.code.length > 0; }
         if (info.id.endsWith("b.js")) { seen.bId = info.id; }
       },
     }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        config: { root: fx.root },
        env: { command: "serve", mode: "development" },
        environment: { name: "client" },
      }),
    ],
    // ~50 bytes of budget: module A's code falls out when B arrives.
    env: { OJ_CACHE_ROOT: fx.root, OJ_INFO_CODE_MB: "0.00005" },
    cwd: fx.root,
  });
  try {
    const pad = `export const pad = "${"x".repeat(80)}";`;
    await host.send({ id: 1, hook: "transform", args: [pad, path.join(fx.root, "a.js")] });
    await host.send({ id: 2, hook: "transform", args: [pad, path.join(fx.root, "b.js")] });
    const res = await host.send({ id: 3, hook: "transform", args: ["", path.join(fx.root, "probe.js")] });
    const seen = JSON.parse(JSON.parse(res.result).code.replace(/^export default /, "").replace(/;$/, ""));
    assert.equal(seen.aCodeValue, null, "A's code left the window");
    assert.deepEqual(seen.aMeta, { tag: "A" }, "A's meta survives eviction");
    assert.equal(seen.bHasCode, true, "the most recent module's code is readable");
  } finally {
    host.close();
    fx.cleanup();
  }
});
