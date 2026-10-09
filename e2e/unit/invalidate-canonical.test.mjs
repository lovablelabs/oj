// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// A runner-backed environment's module graph keys files by their canonical
// path: Vite's resolver realpaths every file it resolves (resolve.ts
// getRealPath, preserveSymlinks off by default). The watcher reports the
// spelling it watched, so an edit to a file reached through a symlink (a
// linked workspace package, a symlinked app dir) used to match no module and
// the runner served the old copy until a restart. The invalidation must retry
// the real path.

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

test("a change spelled through a symlink invalidates the canonically keyed module", async () => {
  const fx = tmpProject({ prefix: "oj-inv-canon-" });
  // shared/route.ts is the real file; src is a symlink to shared, so the
  // watcher spelling <root>/src/route.ts and the canonical spelling diverge.
  fx.write("shared/route.ts", "export const r = 1;\n");
  fs.symlinkSync(path.join(fx.root, "shared"), path.join(fx.root, "src"));
  const watched = path.join(fx.root, "src", "route.ts");
  const canonical = fs.realpathSync(watched);
  assert.notEqual(canonical, watched, "the two spellings must diverge for this test to mean anything");

  // A stub vite whose worker environment keys the module graph by the
  // canonical file, exactly as a real Vite environment does.
  fx.pkg("vite", "index.mjs", {
    "index.mjs": `
      import { realpathSync } from "node:fs";
      export function resolveConfig(inline) {
        const root = inline.root;
        const file = realpathSync(root + "/src/route.ts");
        const entry = { id: "\\0virtual:worker-entry", url: "virtual:worker-entry", file: null, type: "js",
                        importers: new Set(), acceptedHmrDeps: new Set(), acceptedHmrExports: null,
                        isSelfAccepting: true, importedBindings: null, invalidations: [] };
        const route = { id: file, url: "/src/route.ts", file, type: "js",
                        importers: new Set([entry]), acceptedHmrDeps: new Set(), acceptedHmrExports: null,
                        isSelfAccepting: false, importedBindings: null, invalidations: [] };
        const byFile = new Map([[file, new Set([route])]]);
        return {
          root,
          logger: { info() {}, warn() {}, warnOnce() {}, error() {} },
          environments: {
            worker: {
              dev: {
                createEnvironment: (name) => {
                  const sends = [];
                  return {
                    name,
                    __sends: sends,
                    __asked: [],
                    __route: route,
                    moduleGraph: {
                      getModulesByFile(f) { this.__self.__asked.push(f); return byFile.get(f); },
                      onFileChange() {},
                      invalidateModule(mod, seen, ts, isHmr) { mod.invalidations.push({ ts, isHmr }); },
                    },
                    hot: { send: (p) => sends.push(p), on() {}, handleInvoke() {} },
                    init() {},
                  };
                },
              },
            },
          },
        };
      }
      export class DevEnvironment {}
    `,
  });
  // getModulesByFile needs the env to record what it was asked; wire the
  // back-reference in configureServer, where the built env is reachable.
  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "fake-runner:dev",
       config: () => ({ environments: { worker: { dev: { createEnvironment: () => ({}) } } } }),
       configureServer(server) {
         const w = server.environments.worker;
         w.moduleGraph.__self = w;
         server.middlewares.use("/__probe", (req, res) => {
           res.setHeader("content-type", "application/json");
           res.end(JSON.stringify({
             asked: w.__asked,
             sends: w.__sends,
             invalidations: w.__route.invalidations,
           }));
         });
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
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });

  try {
    const info = JSON.parse((await host.send({ id: 1, hook: "getServeInfo" })).result);
    const port = Number(info.middlewarePort);
    assert.ok(port > 0, "middleware server is up");

    // The change arrives spelled through the symlink, as the watcher saw it.
    const res = await fetch(`http://127.0.0.1:${port}/__oj_invalidate`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ paths: [watched] }),
    });
    assert.equal(res.status, 204);

    const seen = await (await fetch(`http://127.0.0.1:${port}/__probe`)).json();
    assert.ok(
      seen.asked.includes(canonical),
      `the lookup must retry the canonical spelling, asked: ${JSON.stringify(seen.asked)}`,
    );
    assert.equal(seen.invalidations.length, 1, "the canonically keyed module was invalidated");
    const update = seen.sends.find((p) => p.type === "update");
    assert.ok(update, `an update must reach the runner, got ${JSON.stringify(seen.sends)}`);
    assert.equal(update.updates[0].acceptedPath, "/@id/virtual:worker-entry");
    assert.ok(
      !host.stderr().includes("matched no module"),
      `the change must not be reported unmatched:\n${host.stderr()}`,
    );
  } finally {
    host.close();
    fx.cleanup();
  }
});
