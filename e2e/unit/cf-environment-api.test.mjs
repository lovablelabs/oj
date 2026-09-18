// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// @cloudflare/vite-plugin (and any Environment-API plugin) subclasses
// vite.DevEnvironment and reads `server.environments[env].initRunner` plus a
// callable `server.middlewares` connect app. oj does not depend on Vite: its
// plugin host loads the *app's* installed Vite, resolves a real config, and
// builds each `server.environments[name]` via that env's
// `dev.createEnvironment(name, config, { ws })`. The path activates the way
// Vite decides it: a plugin DECLARES its dev runtime by returning
// `environments.<name>.dev.createEnvironment` from its `config` hook (no
// vendor name check). This pins that wiring with a stub `vite` in the fixture
// (no real Vite/Miniflare needed).

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

// The plugin here is deliberately NOT cloudflare-named: any plugin declaring
// an environment activates the Environment-API path.
test("an environment-declaring plugin gets server.environments (built from the app's Vite) and a callable middlewares app", async () => {
  const fx = tmpProject({ prefix: "oj-cf-envapi-" });

  // Stub the app's installed Vite: resolveConfig returns two environments,
  // each with a dev.createEnvironment factory the host must call.
  fx.pkg("vite", "index.mjs", {
    "index.mjs": `
      export function resolveConfig(inline) {
        // Track Vite's server startup calls.
        const calls = (globalThis.__ojStubCalls = { clientBuildStart: 0, listened: [] });
        const mkEnv = (extra) => (name) => ({ name, init() {}, listen() { calls.listened.push(name); }, hot: { handleInvoke() {}, on() {} }, ...extra(name) });
        return {
          root: inline.root,
          logger: { info() {}, warn() {}, warnOnce() {}, error() {} },
          environments: {
            client: { dev: { createEnvironment: mkEnv((name) => ({ pluginContainer: { buildStart() { calls.clientBuildStart++; } } })) } },
            worker: { dev: { createEnvironment: mkEnv((name) => ({ initRunner() {} })) } },
          },
        };
      }
      export class DevEnvironment {}
      export function createRunnableDevEnvironment(name) { return { name, init() {} }; }
    `,
  });

  fx.write(
    "oj.plugins.mjs",
    `let seen = {};
     export default [{
       name: "acme-workerd:dev",
       config: () => ({ environments: { worker: { dev: { createEnvironment: () => ({}) } } } }),
       configureServer(server) {
         seen.middlewaresCallable = typeof server.middlewares === "function";
         seen.middlewaresHasUse = typeof server.middlewares.use === "function";
         seen.hasClose = typeof server.close === "function";
         seen.hasTransformIndexHtml = typeof server.transformIndexHtml === "function";
         seen.hasEnvironments = !!server.environments && typeof server.environments === "object";
         seen.envNames = server.environments ? Object.keys(server.environments) : [];
         seen.workerHasInitRunner =
           !!(server.environments && server.environments.worker &&
              typeof server.environments.worker.initRunner === "function");
         // a registered middleware runs when the app is invoked as a function
         let ran = false;
         server.middlewares.use((req, res, next) => { ran = true; next(); });
         server.middlewares({ url: "/x" }, { setHeader() {}, end() {} }, () => {});
         seen.middlewareRan = ran;
         // Like connect, trim trailing slashes and store "" for use(fn).
         // Nitro uses a nonempty route to detect mounted middleware.
         let prefixed = 0;
         server.middlewares.use("/mounted/", (req, res, next) => { prefixed++; next(); });
         server.middlewares({ url: "/x" }, { setHeader() {}, end() {} }, () => {});
         server.middlewares({ url: "/mounted/y" }, { setHeader() {}, end() {} }, () => {});
         // Match queries, extensions and any case, but not a longer path segment.
         server.middlewares({ url: "/mountedx" }, { setHeader() {}, end() {} }, () => {});
         server.middlewares({ url: "/Mounted?q=1" }, { setHeader() {}, end() {} }, () => {});
         server.middlewares({ url: "/mounted.json" }, { setHeader() {}, end() {} }, () => {});
         seen.routes = server.middlewares.stack.map((e) => e.route);
         seen.prefixedRuns = prefixed;
       },
       transform(code, id) {
         if (id.endsWith("probe.js")) return "export default " + JSON.stringify({ ...seen, calls: globalThis.__ojStubCalls }) + ";";
         return null;
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
    const res = await host.send({ id: 1, hook: "transform", args: ["", path.join(fx.root, "probe.js")] });
    const seen = JSON.parse(JSON.parse(res.result).code.replace(/^export default /, "").replace(/;$/, ""));

    // The Environment API: the host called the app's Vite and built one
    // DevEnvironment per resolved environment via its dev.createEnvironment.
    assert.equal(seen.hasEnvironments, true, "configureServer sees server.environments");
    assert.deepEqual(seen.envNames.sort(), ["client", "worker"], "one environment per resolved config env");
    assert.equal(seen.workerHasInitRunner, true, "the worker env is the plugin's own instance (has initRunner)");

    // server.middlewares is a callable connect app AND has .use().
    assert.equal(seen.middlewaresCallable, true, "server.middlewares is callable");
    assert.equal(seen.middlewaresHasUse, true, "server.middlewares.use exists");
    assert.equal(seen.middlewareRan, true, "a registered middleware runs when the app is invoked");
    assert.deepEqual(seen.routes.slice(0, 2), ["", "/mounted"], "routes are stored the way connect stores them");
    assert.equal(seen.prefixedRuns, 3, "a mounted middleware runs under its prefix at a path boundary only (not /mountedx)");

    // Run client buildStart, then listen() on server environments only.
    // oj serves the client itself.
    assert.equal(seen.calls.clientBuildStart, 1, "client buildStart ran once");
    assert.deepEqual(seen.calls.listened, ["worker"], "server environments are listened, the client is not");

    // Surface the cloudflare plugin also reads.
    assert.equal(seen.hasClose, true, "server.close is provided");
    assert.equal(seen.hasTransformIndexHtml, true, "server.transformIndexHtml is provided");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("a source edit sends a targeted update to the accept boundary; dead ends still full-reload", async () => {
  const fx = tmpProject({ prefix: "oj-cf-hmr-" });

  // Stub vite whose worker environment carries a small module graph:
  //   entry (self-accepting, the cloudflare plugin's worker entry shape)
  //     <- route.ts   (update must stop at the entry boundary)
  //   dead.ts         (no importers, no boundary: full-reload)
  //   ignored.ts      (a hotUpdate hook filters it out: nothing sent)
  fx.pkg("vite", "index.mjs", {
    "index.mjs": `
      function node(id, url, file, extra) {
        return { id, url, file, type: "js", importers: new Set(), acceptedHmrDeps: new Set(),
                 acceptedHmrExports: null, isSelfAccepting: false, importedBindings: null,
                 invalidations: [], ...extra };
      }
      export function resolveConfig(inline) {
        const root = inline.root;
        const entry = node("\\0virtual:worker-entry", "virtual:worker-entry", null, { isSelfAccepting: true });
        const route = node(root + "/src/route.ts", "/src/route.ts", root + "/src/route.ts");
        const dead = node(root + "/src/dead.ts", "/src/dead.ts", root + "/src/dead.ts");
        const ignored = node(root + "/src/ignored.ts", "/src/ignored.ts", root + "/src/ignored.ts");
        route.importers.add(entry);
        const byFile = new Map([[route.file, new Set([route])], [dead.file, new Set([dead])], [ignored.file, new Set([ignored])]]);
        const mkEnv = (name, graphed) => {
          const sends = [];
          return {
            name,
            __sends: sends,
            __nodes: { entry, route, dead },
            __hotUpdateFiles: [],
            moduleGraph: {
              onFileChanges: [],
              getModulesByFile(f) { return graphed ? byFile.get(f) : undefined; },
              onFileChange(f) { this.onFileChanges.push(f); },
              invalidateModule(mod, seen, ts, isHmr) { mod.invalidations.push({ ts, isHmr }); },
            },
            hot: { send: (p) => sends.push(p), on() {}, handleInvoke() {} },
            init() {},
          };
        };
        return {
          root,
          logger: { info() {}, warn() {}, warnOnce() {}, error() {} },
          environments: {
            client: { consumer: "client", dev: { createEnvironment: (name) => mkEnv(name, false) } },
            worker: {
              dev: {
                createEnvironment: (name, rc) => {
                  const env = mkEnv(name, true);
                  env.__config = rc;
                  env.plugins = [{
                    name: "hu",
                    hotUpdate(options) {
                      env.__hotUpdateFiles.push(options.file);
                      if (options.file.endsWith("ignored.ts")) return [];
                    },
                  }];
                  return env;
                },
              },
            },
          },
        };
      }
      export class DevEnvironment {}
    `,
  });

  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "vite-plugin-cloudflare:dev",
       config: () => ({ environments: { worker: { dev: { createEnvironment: () => ({}) } } } }),
       configureServer(server) {
         server.middlewares.use("/__probe", (req, res) => {
           const w = server.environments.worker;
           res.setHeader("content-type", "application/json");
           res.end(JSON.stringify({
             workerSends: w.__sends,
             clientSends: server.environments.client.__sends,
             onFileChanges: w.moduleGraph.onFileChanges,
             hotUpdateFiles: w.__hotUpdateFiles,
             routeInvalidations: w.__nodes.route.invalidations,
             deadInvalidations: w.__nodes.dead.invalidations,
             preTransform: {
               worker: w.__config.environments.worker.dev.preTransformRequests ?? null,
               client: w.__config.environments.client.dev.preTransformRequests ?? null,
             },
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
    assert.equal(info.runnerEnvironments, true, "the host reports built runner environments");

    const invalidate = async (rel) => {
      const res = await fetch(`http://127.0.0.1:${port}/__oj_invalidate`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ paths: [path.join(fx.root, rel)] }),
      });
      assert.equal(res.status, 204);
    };
    const probe = async () => (await fetch(`http://127.0.0.1:${port}/__probe`)).json();

    // A change inside the graph with a self-accepting entry above it: one
    // targeted update to the entry boundary, no full-reload.
    await invalidate("src/route.ts");
    let seen = await probe();
    assert.equal(seen.workerSends.length, 1, `one payload, got ${JSON.stringify(seen.workerSends)}`);
    const update = seen.workerSends[0];
    assert.equal(update.type, "update");
    assert.equal(update.updates.length, 1);
    assert.equal(update.updates[0].type, "js-update");
    assert.equal(update.updates[0].path, "/@id/virtual:worker-entry");
    assert.equal(update.updates[0].acceptedPath, "/@id/virtual:worker-entry");
    assert.ok(update.updates[0].timestamp > 0, "update carries the timestamp");
    assert.equal(seen.routeInvalidations.length, 1, "the changed module was invalidated");
    assert.equal(seen.routeInvalidations[0].isHmr, true, "invalidated as an HMR invalidation");
    assert.ok(seen.onFileChanges.includes(path.join(fx.root, "src/route.ts")), "moduleGraph.onFileChange ran");
    assert.ok(seen.hotUpdateFiles.includes(path.join(fx.root, "src/route.ts")), "plugin hotUpdate hooks ran");
    assert.deepEqual(seen.clientSends, [], "the client environment had no matching modules: nothing sent");

    // Propagation dead-ends (no importers, no boundary): full-reload, as Vite.
    await invalidate("src/dead.ts");
    seen = await probe();
    assert.equal(seen.workerSends.length, 2);
    assert.equal(seen.workerSends[1].type, "full-reload");
    assert.equal(seen.deadInvalidations.length, 1, "the dead-end module is still invalidated");

    // A hotUpdate hook returning [] suppresses the update entirely.
    await invalidate("src/ignored.ts");
    seen = await probe();
    assert.equal(seen.workerSends.length, 2, "filtered-out change sends nothing");

    // A file outside the graph: nothing to send for this environment.
    await invalidate("src/nowhere.ts");
    seen = await probe();
    assert.equal(seen.workerSends.length, 2, "unknown file sends nothing");

    // preTransformRequests: defaulted on for the server consumer, left alone
    // for the client.
    assert.equal(seen.preTransform.worker, true, "server env gets dev.preTransformRequests");
    assert.equal(seen.preTransform.client, null, "client env is not touched");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("a user-set preTransformRequests is not overridden", async () => {
  const fx = tmpProject({ prefix: "oj-cf-pretransform-" });
  fx.pkg("vite", "index.mjs", {
    "index.mjs": `
      export function resolveConfig(inline) {
        return {
          root: inline.root,
          logger: { info() {}, warn() {}, warnOnce() {}, error() {} },
          environments: {
            worker: { dev: { createEnvironment: (name, rc) => ({ name, init() {}, hot: { send() {}, on() {} }, __config: rc }) } },
          },
        };
      }
      export class DevEnvironment {}
    `,
  });
  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "vite-plugin-cloudflare:dev",
       config: () => ({ environments: { worker: { dev: { createEnvironment: () => ({}) } } } }),
       configureServer(server) {
         server.middlewares.use("/__probe", (req, res) => {
           res.setHeader("content-type", "application/json");
           res.end(JSON.stringify({
             worker: server.environments.worker.__config.environments.worker.dev.preTransformRequests ?? null,
           }));
         });
       },
     }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        // The user's config chose a value: the host must not flip it.
        config: { root: fx.root, environments: { worker: { dev: { preTransformRequests: false } } } },
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
    const seen = await (await fetch(`http://127.0.0.1:${port}/__probe`)).json();
    assert.equal(seen.worker, null, "the resolved value stands; the host does not force preTransformRequests");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("a non-cloudflare plugin set gets oj-backed client/ssr stand-ins, not Vite-built environments", async () => {
  const fx = tmpProject({ prefix: "oj-cf-noenv-" });
  // No Vite in node_modules and no cloudflare plugin: buildEnvironments must not
  // run (nothing throws), yet server.environments still has client and ssr, as
  // Vite's createServer always provides them.
  fx.write(
    "oj.plugins.mjs",
    `let seen = {};
     export default [{
       name: "plain-plugin",
       configureServer(server) {
         seen.envNames = Object.keys(server.environments).sort();
         seen.stubbed = server.environments.client.__ojStub === true && server.environments.ssr.__ojStub === true;
         seen.middlewaresCallable = typeof server.middlewares === "function";
       },
       transform(code, id) {
         if (id.endsWith("probe.js")) return "export default " + JSON.stringify(seen) + ";";
         return null;
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
    const res = await host.send({ id: 1, hook: "transform", args: ["", path.join(fx.root, "probe.js")] });
    const seen = JSON.parse(JSON.parse(res.result).code.replace(/^export default /, "").replace(/;$/, ""));
    assert.deepEqual(seen.envNames, ["client", "ssr"], "client and ssr are always exposed");
    assert.equal(seen.stubbed, true, "they are oj stand-ins, no Vite environment construction ran");
    assert.equal(seen.middlewaresCallable, true, "callable middlewares still provided to every app");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("create retries failed resolutions, delete prunes, legacy hooks and hook errors follow Vite", async () => {
  const fx = tmpProject({ prefix: "oj-cf-hmr2-" });

  // Stub vite whose worker graph has a self-accepting entry importing:
  //   broken.ts  - not in the graph by file, but in _hasResolveFailedErrorModules
  //   gone.ts    - deleted (onFileDelete, then the entry boundary updates)
  //   errfile.ts - a hotUpdate hook throws (the env gets {type:"error"})
  // The client env carries a legacy handleHotUpdate-only plugin.
  fx.pkg("vite", "index.mjs", {
    "index.mjs": `
      function node(id, url, file, extra) {
        return { id, url, file, type: "js", importers: new Set(), acceptedHmrDeps: new Set(),
                 acceptedHmrExports: null, isSelfAccepting: false, importedBindings: null,
                 invalidations: [], ...extra };
      }
      export function resolveConfig(inline) {
        const root = inline.root;
        const entry = node("\\0virtual:worker-entry", "virtual:worker-entry", null, { isSelfAccepting: true });
        const broken = node(root + "/src/broken.ts", "/src/broken.ts", root + "/src/broken.ts");
        const gone = node(root + "/src/gone.ts", "/src/gone.ts", root + "/src/gone.ts");
        const errf = node(root + "/src/errfile.ts", "/src/errfile.ts", root + "/src/errfile.ts");
        for (const n of [broken, gone, errf]) n.importers.add(entry);
        const byFile = new Map([[gone.file, new Set([gone])], [errf.file, new Set([errf])]]);
        const mkWorker = (name) => {
          const env = {
            name, __sends: [], __watchChanges: [], __deletes: [], __nodes: { broken, gone },
            pluginContainer: { watchChange(f, m) { env.__watchChanges.push(m.event + ":" + f.split("/").pop()); } },
            moduleGraph: {
              _hasResolveFailedErrorModules: new Set([broken]),
              getModulesByFile(f) { return byFile.get(f); },
              onFileChange() {},
              onFileDelete(f) { env.__deletes.push(f); },
              invalidateModule(mod, seen, ts, isHmr) { mod.invalidations.push({ ts, isHmr }); },
            },
            plugins: [{ name: "boom", hotUpdate(o) { if (o.file.endsWith("errfile.ts")) throw new Error("hook exploded"); } }],
            hot: { send: (p) => env.__sends.push(p), on() {}, handleInvoke() {} },
            init() {},
          };
          return env;
        };
        const mkClient = (name) => {
          const env = {
            name, __sends: [], __legacy: [],
            plugins: [{ name: "old", handleHotUpdate(ctx) { env.__legacy.push(ctx.file); } }],
            moduleGraph: { getModulesByFile() { return undefined; }, onFileChange() {}, invalidateModule() {} },
            hot: { send: (p) => env.__sends.push(p), on() {} },
            init() {},
          };
          return env;
        };
        return {
          root,
          logger: { info() {}, warn() {}, warnOnce() {}, error() {} },
          environments: {
            client: { consumer: "client", dev: { createEnvironment: mkClient } },
            worker: { dev: { createEnvironment: mkWorker } },
          },
        };
      }
      export class DevEnvironment {}
    `,
  });

  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "vite-plugin-cloudflare:dev",
       config: () => ({ environments: { worker: { dev: { createEnvironment: () => ({}) } } } }),
       configureServer(server) {
         server.middlewares.use("/__probe", (req, res) => {
           const w = server.environments.worker;
           const c = server.environments.client;
           res.setHeader("content-type", "application/json");
           res.end(JSON.stringify({
             workerSends: w.__sends,
             watchChanges: w.__watchChanges,
             deletes: w.__deletes,
             brokenInvalidations: w.__nodes.broken.invalidations,
             legacy: c.__legacy,
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

    const invalidate = async (rel, type) => {
      const res = await fetch(`http://127.0.0.1:${port}/__oj_invalidate`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ changes: [{ path: path.join(fx.root, rel), type }] }),
      });
      assert.equal(res.status, 204);
    };
    const probe = async () => (await fetch(`http://127.0.0.1:${port}/__probe`)).json();

    // A created file retries the modules whose resolution previously failed:
    // new.ts is not in the graph, but broken.ts is retried and its update
    // reaches the self-accepting entry boundary.
    await invalidate("src/new.ts", "create");
    let seen = await probe();
    assert.equal(seen.workerSends.length, 1, `one payload, got ${JSON.stringify(seen.workerSends)}`);
    assert.equal(seen.workerSends[0].type, "update");
    assert.equal(seen.workerSends[0].updates[0].path, "/@id/virtual:worker-entry");
    assert.equal(seen.brokenInvalidations.length, 1, "the failed-resolution module was invalidated");
    assert.ok(seen.watchChanges.includes("create:new.ts"), "watchChange saw the create event");

    // A deleted file goes through onFileDelete and still updates its boundary.
    await invalidate("src/gone.ts", "delete");
    seen = await probe();
    assert.ok(seen.deletes.some((f) => f.endsWith("gone.ts")), "moduleGraph.onFileDelete ran");
    assert.equal(seen.workerSends.length, 2);
    assert.equal(seen.workerSends[1].type, "update");

    // A throwing hotUpdate hook: the environment gets {type:"error"}, as Vite's
    // hmr() sends, not a blind full-reload.
    await invalidate("src/errfile.ts", "update");
    seen = await probe();
    assert.equal(seen.workerSends.length, 3);
    assert.equal(seen.workerSends[2].type, "error");
    assert.ok(seen.workerSends[2].err.message.includes("hook exploded"), "the error payload carries the message");

    // Legacy handleHotUpdate: dispatched for the client environment on updates
    // only, as in Vite.
    assert.ok(seen.legacy.some((f) => f.endsWith("errfile.ts")), "legacy hook ran for the update");
    assert.ok(!seen.legacy.some((f) => f.endsWith("new.ts")), "legacy hook is not called for create events");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("invalidate dedup: content identity, atomic-save create, read errors", async () => {
  const fx = tmpProject({ prefix: "oj-cf-dedup-" });
  // A real file, so the host's content-identity dedup can read it: the early
  // (pre-settle) and settled sends for one write carry the same (path, type)
  // and the same bytes.
  fx.write("src/live.ts", "export const live = 1;\n");

  // Stub vite: a worker graph whose self-accepting entry imports live.ts, so
  // every processed change sends exactly one targeted update.
  fx.pkg("vite", "index.mjs", {
    "index.mjs": `
      function node(id, url, file, extra) {
        return { id, url, file, type: "js", importers: new Set(), acceptedHmrDeps: new Set(),
                 acceptedHmrExports: null, isSelfAccepting: false, importedBindings: null,
                 invalidations: [], ...extra };
      }
      export function resolveConfig(inline) {
        const root = inline.root;
        const entry = node("\\0virtual:worker-entry", "virtual:worker-entry", null, { isSelfAccepting: true });
        const live = node(root + "/src/live.ts", "/src/live.ts", root + "/src/live.ts");
        live.importers.add(entry);
        const byFile = new Map([[live.file, new Set([live])]]);
        return {
          root,
          logger: { info() {}, warn() {}, warnOnce() {}, error() {} },
          environments: {
            worker: {
              dev: {
                createEnvironment: (name) => {
                  const env = {
                    name, __sends: [],
                    moduleGraph: {
                      getModulesByFile(f) { return byFile.get(f); },
                      onFileChange() {},
                      invalidateModule() {},
                    },
                    hot: { send: (p) => env.__sends.push(p), on() {}, handleInvoke() {} },
                    init() {},
                  };
                  return env;
                },
              },
            },
          },
        };
      }
      export class DevEnvironment {}
    `,
  });

  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "vite-plugin-cloudflare:dev",
       config: () => ({ environments: { worker: { dev: { createEnvironment: () => ({}) } } } }),
       configureServer(server) {
         const emits = [];
         server.watcher.on("change", (f) => emits.push("change:" + f.split("/").pop()));
         server.watcher.on("add", (f) => emits.push("add:" + f.split("/").pop()));
         server.middlewares.use("/__probe", (req, res) => {
           res.setHeader("content-type", "application/json");
           res.end(JSON.stringify({ workerSends: server.environments.worker.__sends, emits }));
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

    const file = path.join(fx.root, "src", "live.ts");
    const invalidate = async (p = file, type = "update") => {
      const res = await fetch(`http://127.0.0.1:${port}/__oj_invalidate`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ changes: [{ path: p, type }] }),
      });
      assert.equal(res.status, 204);
    };
    const probe = async () => (await fetch(`http://127.0.0.1:${port}/__probe`)).json();

    // The early and settled sends for one write: same (path, type), same
    // content. The second is skipped, so the hooks/update run once and the
    // watcher sees one event, as under Vite's single chokidar event.
    const fixed = new Date("2026-01-01T00:00:00Z");
    fs.utimesSync(file, fixed, fixed);
    await invalidate();
    await invalidate();
    let seen = await probe();
    assert.equal(seen.workerSends.length, 1, `one hot payload, got ${JSON.stringify(seen.workerSends)}`);
    assert.deepEqual(seen.emits, ["change:live.ts"], "one watcher emit for the repeated identical send");

    // DIFFERENT content under the SAME mtime (an mtime heuristic's blind spot:
    // coarse timestamps, two writes in one tick): the hash differs, so the
    // settled send carrying genuinely-new content must process again.
    fs.writeFileSync(file, "export const live = 2;\n");
    fs.utimesSync(file, fixed, fixed);
    await invalidate();
    seen = await probe();
    assert.equal(seen.workerSends.length, 2, "different content under the same mtime processes again");
    assert.equal(seen.emits.length, 2, "the watcher sees the second, changed write");

    // An atomic save's recreate half: a "create" for a file the worker graph
    // already knows is handled as an update (chokidar reports these as
    // change), so update-then-create is one pass and no phantom "add" reaches
    // plugin watchers.
    fs.writeFileSync(file, "export const live = 3;\n");
    await invalidate(file, "update");
    await invalidate(file, "create");
    seen = await probe();
    assert.equal(seen.workerSends.length, 3, "update-then-create for known content is one pass");
    assert.ok(!seen.emits.some((e) => e.startsWith("add:")), `no phantom add emit, got ${JSON.stringify(seen.emits)}`);

    // A file that cannot be read (never suppress on error): both sends process.
    const ghost = path.join(fx.root, "src", "ghost.ts");
    await invalidate(ghost, "update");
    await invalidate(ghost, "update");
    seen = await probe();
    const ghostEmits = seen.emits.filter((e) => e === "change:ghost.ts");
    assert.equal(ghostEmits.length, 2, "a read error processes every time");
  } finally {
    host.close();
    fx.cleanup();
  }
});

// The extractor's verdict travels in the spawn payload as `initial.runnerBacked`.
// Trust shape: extraction TRUE is authoritative and sufficient; extraction
// FALSE or ABSENT falls through to the host's own declaration check, so a
// missing or stale-false verdict can never silently disable the worker path
// when the host itself can see the declaration.
test("initial.runnerBacked=true keeps the Environment-API path even when the declaring plugin's config hook throws in the host", async () => {
  const fx = tmpProject({ prefix: "oj-cf-extauth-" });
  fx.pkg("vite", "index.mjs", {
    "index.mjs": `
      export function resolveConfig(inline) {
        const mkEnv = () => (name) => ({ name, init() {}, hot: { handleInvoke() {}, on() {} } });
        return {
          root: inline.root,
          logger: { info() {}, warn() {}, warnOnce() {}, error() {} },
          environments: { worker: { dev: { createEnvironment: mkEnv() } } },
        };
      }
      export class DevEnvironment {}
    `,
  });
  // The plugin's config hook throws IN THE HOST, so the host's own
  // declaration sources (pluginConfigDelta) never see the declaration — the
  // extractor's independent run already decided, and buildEnvironments
  // re-resolves on fresh instances (the stub vite), where it succeeds.
  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "acme-workerd:dev",
       config: () => { throw new Error("hook exploded in the host"); },
     }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        config: { root: fx.root },
        env: { command: "serve", mode: "development" },
        environment: { name: "client" },
        ojStartMode: true,
        runnerBacked: true,
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const info = await host.serveInfo();
    assert.equal(info.runnerEnvironments, true, "environments built although the host-side hook threw");
    assert.match(host.stderr(), /decided by config extraction/, "the deciding source is logged");
    assert.match(host.stderr(), /config\(acme-workerd:dev\) skipped/, "the host-side hook failure is on stderr");
    assert.doesNotMatch(host.stderr(), /none came up/);
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("initial.runnerBacked=false falls through to the host's own declaration check", async () => {
  const fx = tmpProject({ prefix: "oj-cf-extfalse-" });
  fx.pkg("vite", "index.mjs", {
    "index.mjs": `
      export function resolveConfig(inline) {
        const mkEnv = () => (name) => ({ name, init() {}, hot: { handleInvoke() {}, on() {} } });
        return {
          root: inline.root,
          logger: { info() {}, warn() {}, warnOnce() {}, error() {} },
          environments: { worker: { dev: { createEnvironment: mkEnv() } } },
        };
      }
      export class DevEnvironment {}
    `,
  });
  // A declaring plugin whose hook SUCCEEDS in the host while extraction said
  // false (a degraded extraction path, a stale cached verdict): the false is
  // NOT final — the host's own declaration check decides, and the worker path
  // stays up instead of being silently disabled by a wrong verdict.
  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "acme-workerd:dev",
       config: () => ({ environments: { worker: { dev: { createEnvironment: () => ({}) } } } }),
     }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        config: { root: fx.root },
        env: { command: "serve", mode: "development" },
        environment: { name: "client" },
        ojStartMode: true,
        runnerBacked: false,
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const info = await host.serveInfo();
    assert.equal(info.runnerEnvironments, true, "the host's own check overrides extraction's false");
    assert.match(
      host.stderr(),
      /decided by host declaration check \(extraction said no\)/,
      "the deciding source (and the disagreement) is logged",
    );
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("initial.runnerBacked=false with no host-visible declaration keeps the path off", async () => {
  const fx = tmpProject({ prefix: "oj-cf-extfalse-plain-" });
  fx.write("oj.plugins.mjs", 'export default [{ name: "inert" }];\n');
  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        config: { root: fx.root },
        env: { command: "serve", mode: "development" },
        environment: { name: "client" },
        runnerBacked: false,
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const info = await host.serveInfo();
    assert.equal(info.runnerEnvironments, false);
    assert.doesNotMatch(host.stderr(), /custom dev environment is declared/);
    assert.doesNotMatch(host.stderr(), /none came up/, "nothing declared anywhere: nothing was expected to come up");
  } finally {
    host.close();
    fx.cleanup();
  }
});

// The "declared but none came up" warning is hoisted out of the build block:
// it fires whenever detection said true and no environment exists afterwards,
// whatever failed in between (here the app has no vite at all to build with).
test("detection true with nothing built prints the none-came-up warning, whatever the failure", async () => {
  const fx = tmpProject({ prefix: "oj-cf-nonecameup-" });
  fx.write("oj.plugins.mjs", 'export default [{ name: "inert" }];\n');
  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        config: { root: fx.root },
        env: { command: "serve", mode: "development" },
        environment: { name: "client" },
        runnerBacked: true,
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const info = await host.serveInfo();
    assert.equal(info.runnerEnvironments, false);
    // The warning is written to stderr independently of the serve-info push,
    // so poll for it rather than assuming it flushed by the time serveInfo
    // resolves (that race made this test flaky under CI load).
    const warning = /a plugin declared a custom dev environment but none came up/;
    for (let i = 0; i < 100 && !warning.test(host.stderr()); i++) {
      await new Promise((r) => setTimeout(r, 50));
    }
    assert.match(
      host.stderr(),
      warning,
      "silent degradation to the Node SSR runner is never acceptable",
    );
  } finally {
    host.close();
    fx.cleanup();
  }
});

// The `{resync:true}` catch-up is ACKed on ENQUEUE and coalesced: the
// invalidate queue is serialized, so a resync stuck behind a slow hotUpdate
// used to hold the caller's POST past its client timeout — the retry then
// stacked a SECOND full-reload and the caller logged a false "edits may be
// stale" warning. Now the ACK returns the moment the resync is queued (fast
// even behind a slow queue), and a pending resync absorbs duplicates: however
// many retries land, exactly one full-reload reaches each environment.
test("resync acks on enqueue and duplicates coalesce behind a slow invalidate queue", async () => {
  const fx = tmpProject({ prefix: "oj-cf-resync-" });
  fx.write("src/live.ts", "export const live = 1;\n");
  fx.pkg("vite", "index.mjs", {
    "index.mjs": `
      function node(id, url, file, extra) {
        return { id, url, file, type: "js", importers: new Set(), acceptedHmrDeps: new Set(),
                 acceptedHmrExports: null, isSelfAccepting: false, importedBindings: null,
                 invalidations: [], ...extra };
      }
      export function resolveConfig(inline) {
        const root = inline.root;
        const entry = node("\\0virtual:worker-entry", "virtual:worker-entry", null, { isSelfAccepting: true });
        const live = node(root + "/src/live.ts", "/src/live.ts", root + "/src/live.ts");
        live.importers.add(entry);
        const byFile = new Map([[live.file, new Set([live])]]);
        return {
          root,
          logger: { info() {}, warn() {}, warnOnce() {}, error() {} },
          environments: {
            worker: {
              dev: {
                createEnvironment: (name) => {
                  const env = {
                    name, __sends: [],
                    // A slow hotUpdate hook: one change invalidation occupies
                    // the serialized queue for ~700 ms.
                    plugins: [{ name: "slow", hotUpdate: () => new Promise((r) => setTimeout(r, 700)) }],
                    moduleGraph: {
                      getModulesByFile(f) { return byFile.get(f); },
                      onFileChange() {},
                      invalidateAll() { env.__sends.push({ type: "__invalidateAll" }); },
                      invalidateModule() {},
                    },
                    hot: { send: (p) => env.__sends.push(p), on() {}, handleInvoke() {} },
                    init() {},
                  };
                  return env;
                },
              },
            },
          },
        };
      }
      export class DevEnvironment {}
    `,
  });
  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "vite-plugin-cloudflare:dev",
       config: () => ({ environments: { worker: { dev: { createEnvironment: () => ({}) } } } }),
       configureServer(server) {
         server.middlewares.use("/__probe", (req, res) => {
           res.setHeader("content-type", "application/json");
           res.end(JSON.stringify({ workerSends: server.environments.worker.__sends }));
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
    const post = (body) =>
      fetch(`http://127.0.0.1:${port}/__oj_invalidate`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });

    // Occupy the queue with a slow change invalidation (answered on drain).
    const slow = post({ changes: [{ path: path.join(fx.root, "src", "live.ts"), type: "update" }] });
    await new Promise((r) => setTimeout(r, 100));

    // Two resyncs behind it: both ACK fast (enqueue, not drain).
    const t0 = Date.now();
    const [r1, r2] = await Promise.all([post({ resync: true }), post({ resync: true })]);
    assert.equal(r1.status, 204);
    assert.equal(r2.status, 204);
    assert.ok(Date.now() - t0 < 500, `resync ACKs on enqueue, took ${Date.now() - t0}ms behind a 700ms queue`);

    assert.equal((await slow).status, 204, "the change invalidation still answers on completion");
    await new Promise((r) => setTimeout(r, 300));
    const seen = await (await fetch(`http://127.0.0.1:${port}/__probe`)).json();
    const reloads = seen.workerSends.filter((p) => p.type === "full-reload");
    assert.equal(reloads.length, 1, `duplicates coalesce into ONE full-reload: ${JSON.stringify(seen.workerSends)}`);
    assert.equal(
      seen.workerSends.filter((p) => p.type === "__invalidateAll").length,
      1,
      "and one whole-graph invalidation",
    );

    // A resync AFTER the pending one ran is a fresh one, not absorbed forever.
    await post({ resync: true });
    await new Promise((r) => setTimeout(r, 200));
    const again = await (await fetch(`http://127.0.0.1:${port}/__probe`)).json();
    assert.equal(again.workerSends.filter((p) => p.type === "full-reload").length, 2);
  } finally {
    host.close();
    fx.cleanup();
  }
});
