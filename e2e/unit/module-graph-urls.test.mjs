// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Module-graph nodes carry the module's served URL beside its resolved id, as
// Vite's do: HMR update frames are built from `node.url` and the client
// matches them against the URLs it imported, so an absolute fs path there
// makes every update a silent no-op. A plain app file maps to its
// root-relative URL (or /@fs outside the root); virtuals, queries and
// dependency files keep their id. `urlToModuleMap` and `getModuleByUrl` must
// answer by that URL.

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import os from "node:os";
import { rpcSidecar, tmpProject } from "./harness.mjs";

test("graph nodes map plain app files to served urls and stay addressable by them", async () => {
  const fx = tmpProject({ prefix: "oj-graphurl-" });
  fx.write("widget.js", "export const w = 1;\n");
  fs.mkdirSync(path.join(fx.root, "node_modules", "dep"), { recursive: true });
  fs.writeFileSync(path.join(fx.root, "node_modules", "dep", "index.js"), "exports.d = 1;\n");
  const outside = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "oj-outside-")), "linked.js");
  fs.writeFileSync(outside, "export const l = 1;\n");

  const widget = path.join(fx.root, "widget.js");
  const dep = path.join(fx.root, "node_modules", "dep", "index.js");
  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "graph-probe",
       async transform(code, id) {
         if (!id.endsWith("probe.js")) return null;
         const g = this.environment.moduleGraph;
         const app = g.getModuleById(${JSON.stringify(widget)});
         const out = g.getModuleById(${JSON.stringify(outside)});
         const dep = g.getModuleById(${JSON.stringify(dep)});
         const virt = await g.ensureEntryFromUrl("\\0virtual:thing");
         return JSON.stringify({
           appUrl: app.url,
           appId: app.id,
           outUrl: out.url,
           depUrl: dep.url,
           virtUrl: virt.url,
           byUrlMap: g.urlToModuleMap.get(app.url) === app,
           byUrlFn: (await g.getModuleByUrl(app.url)) === app,
           byIdStillWorks: g.getModuleById(${JSON.stringify(widget)}) === app,
         });
       },
     }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ config: { root: fx.root } })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const res = await host.send({ id: 1, hook: "transform", args: ["", path.join(fx.root, "probe.js")] });
    assert.equal(res.error, undefined, res.error);
    const seen = JSON.parse(JSON.parse(res.result).code);
    assert.equal(seen.appUrl, "/widget.js", "an in-root file serves at its root-relative url");
    assert.equal(seen.appId, widget, "the id stays the resolved path");
    assert.equal(seen.outUrl, `/@fs${outside}`, "an outside-root file serves under /@fs");
    assert.equal(seen.depUrl, dep, "a dependency file keeps its id as url");
    assert.equal(seen.virtUrl, "\0virtual:thing", "a virtual id keeps its id as url");
    assert.equal(seen.byUrlMap, true, "urlToModuleMap answers by the served url");
    assert.equal(seen.byUrlFn, true, "getModuleByUrl answers by the served url");
    assert.equal(seen.byIdStillWorks, true, "getModuleById is untouched");
  } finally {
    host.close();
    fx.cleanup();
    fs.rmSync(path.dirname(outside), { recursive: true, force: true });
  }
});
