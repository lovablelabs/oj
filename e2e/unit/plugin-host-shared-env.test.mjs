// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { repo, rpcSidecar, tmpProject } from "./harness.mjs";

// A Start build host, as start_bundle/hosts.rs spawns one.
function startBuildHost(fx) {
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

const events = (fx) =>
  fs.existsSync(path.join(fx.root, "events.log"))
    ? fs.readFileSync(path.join(fx.root, "events.log"), "utf8").trim().split("\n")
    : [];

const LOG = `import { appendFileSync } from "node:fs";
const log = (line) => appendFileSync(new URL("./events.log", import.meta.url), line + "\\n");`;

// Vite's runConfigHook / runConfigEnvironmentHook / configResolved run on
// BasicMinimalPluginContext (version meta, no environment), configEnvironment
// plugin by plugin then environment by environment.
test("config-phase hooks get Vite's minimal context, configEnvironment plugin first", async () => {
  const fx = tmpProject({ prefix: "oj-shared-ctx-" });
  fx.write(
    "oj.plugins.mjs",
    `${LOG}
     const plugin = (name) => ({
       name,
       config() { log(name + " config env=" + (this.environment === undefined) + " rolldown=" + !!this.meta.rolldownVersion); },
       configEnvironment(env) { log(name + " configEnvironment " + env + " env=" + (this.environment === undefined)); },
       configResolved() { log(name + " configResolved env=" + (this.environment === undefined) + " watch=" + this.meta.watchMode); },
     });
     export default [plugin("a"), plugin("b")];\n`,
  );
  const host = startBuildHost(fx);
  try {
    await host.initSignal();
    assert.deepEqual(events(fx), [
      "a config env=true rolldown=true",
      "b config env=true rolldown=true",
      "a configEnvironment client env=true",
      "a configEnvironment ssr env=true",
      "b configEnvironment client env=true",
      "b configEnvironment ssr env=true",
      "a configResolved env=true watch=false",
      "b configResolved env=true watch=false",
    ]);
  } finally {
    host.close();
    fx.cleanup();
  }
});

// Vite's builder with `sharedPlugins`: the server environment builds on the
// client's plugin instances, its own plugin set (applyToEnvironment), its own
// buildStart and emitted files, and moduleParsed comes from the bundler.
test("switchEnvironment moves the shared instances to the next build", async () => {
  const fx = tmpProject({ prefix: "oj-shared-env-" });
  fx.write(
    "oj.plugins.mjs",
    `${LOG}
     let builds = 0;
     export default [
       {
         name: "shared",
         buildStart() { builds++; log("buildStart " + this.environment.name + " builds=" + builds); },
         transform(code, id) { this.emitFile({ type: "asset", fileName: this.environment.name + ".txt", source: "x" }); return null; },
         moduleParsed(info) { log("moduleParsed " + this.environment.name + " " + info.id + " " + info.importedIds.join(",")); },
       },
       { name: "client-only", applyToEnvironment: (e) => e.name === "client", buildStart() { log("client-only " + this.environment.name); } },
     ];\n`,
  );
  const host = startBuildHost(fx);
  try {
    let id = 1;
    await host.send({ id: id++, hook: "buildStart", args: [] });
    await host.send({ id: id++, hook: "transform", args: ["export {}", path.join(fx.root, "a.js")] });
    await host.send({
      id: id++,
      hook: "bundleModuleParsed",
      args: [JSON.stringify({ id: "/a.js", code: "export {}", importedIds: ["/b.js"] })],
    });
    const clientFiles = JSON.parse((await host.send({ id: id++, hook: "getEmittedFiles", args: [] })).result);
    assert.deepEqual(
      clientFiles.map((f) => f.fileName),
      ["client.txt"],
    );
    await host.send({ id: id++, hook: "switchEnvironment", args: ["ssr"] });
    await host.send({ id: id++, hook: "buildStart", args: [] });
    const ssrFiles = JSON.parse((await host.send({ id: id++, hook: "getEmittedFiles", args: [] })).result);
    assert.deepEqual(ssrFiles, [], "the ssr build starts with no emitted files");
    assert.deepEqual(events(fx), [
      "buildStart client builds=1",
      "client-only client",
      // The transform did not fire moduleParsed: the bundler reports it.
      "moduleParsed client /a.js /b.js",
      "buildStart ssr builds=2",
    ]);
  } finally {
    host.close();
    fx.cleanup();
  }
});

// vite:asset without build.emitAssets (ssr's default): its unordered
// generateBundle runs after the order:"pre" hooks and the enforce:"pre"
// plugins' unordered ones and deletes the bundle's assets (JS source maps
// and ssr-manifest.json aside); later emits stay.
test("an ssr generateBundle drops the assets emitted before vite:asset", async () => {
  const fx = tmpProject({ prefix: "oj-shared-assets-" });
  fx.write(
    "oj.plugins.mjs",
    `const emitter = (name, enforce, order) => ({
       name,
       ...(enforce ? { enforce } : {}),
       generateBundle: {
         ...(order ? { order } : {}),
         handler() { this.emitFile({ type: "asset", fileName: name + ".txt", source: name }); },
       },
     });
     export default [
       emitter("pre-plugin", "pre"),
       emitter("pre-hook", undefined, "pre"),
       emitter("pre-plugin-post-hook", "pre", "post"),
       emitter("normal"),
       {
         name: "map",
         enforce: "pre",
         generateBundle() { this.emitFile({ type: "asset", fileName: "x.js.map", source: "{}" }); },
       },
     ];\n`,
  );
  const host = startBuildHost(fx);
  try {
    let id = 1;
    const bundle = {
      "old.css": { type: "asset", fileName: "old.css", source: "a{}" },
      "entry.mjs": { type: "chunk", fileName: "entry.mjs", code: "" },
    };
    const emitted = async () =>
      JSON.parse((await host.send({ id: id++, hook: "getEmittedFiles", args: [] })).result)
        .map((f) => f.fileName)
        .sort();
    // The client keeps everything.
    const client = JSON.parse(
      (await host.send({ id: id++, hook: "generateBundle", args: [JSON.stringify(bundle), "true"] })).result,
    );
    assert.ok(client["old.css"]);
    assert.deepEqual(await emitted(), [
      "normal.txt",
      "pre-hook.txt",
      "pre-plugin-post-hook.txt",
      "pre-plugin.txt",
      "x.js.map",
    ]);

    await host.send({ id: id++, hook: "switchEnvironment", args: ["ssr"] });
    assert.equal((await host.send({ id: id++, hook: "hasGenerateBundle", args: [] })).result, "true");
    const ssr = JSON.parse(
      (await host.send({ id: id++, hook: "generateBundle", args: [JSON.stringify(bundle), "true"] })).result,
    );
    assert.equal(ssr["old.css"], undefined, "the bundle's assets are deleted");
    assert.ok(ssr["entry.mjs"], "chunks stay");
    assert.deepEqual(await emitted(), ["normal.txt", "pre-plugin-post-hook.txt", "x.js.map"]);
  } finally {
    host.close();
    fx.cleanup();
  }
});

// Vite's builder.buildApp: hooks in hook order, the config builder's own
// (oj's environment builds, the buildEnvironments reverse RPC) just before
// the first post-order hook, `builder.build` of an environment oj does not
// build refused, and a throwing hook failing the build.
test("buildApp runs the hooks around oj's environment builds", async () => {
  const fx = tmpProject({ prefix: "oj-shared-app-" });
  fx.write(
    "oj.plugins.mjs",
    `${LOG}
     export default [
       { name: "post", buildApp: { order: "post", async handler(b) {
           log("post built=" + b.environments.client.isBuilt + "," + b.environments.ssr.isBuilt + " outDir=" + b.environments.ssr.config.build.outDir);
           await b.build(b.environments.client);
           try { await b.build("nitro"); } catch (e) { log("nitro: " + e.message); }
           if (process.env.OJ_TEST_THROW) this.error("refused");
         } } },
       { name: "pre", buildApp: { order: "pre", handler(b) { log("pre this.environment=" + (this.environment === undefined) + " built=" + b.environments.client.isBuilt); } } },
     ];\n`,
  );
  const built = JSON.stringify({
    client: { outDir: "/out/client", input: "c.tsx" },
    ssr: { outDir: "/out", input: "s.tsx" },
  });
  const run = async (env) => {
    const host = rpcSidecar("plugin-host.mjs", {
      args: [
        path.join(fx.root, "oj.plugins.mjs"),
        JSON.stringify({
          config: { root: fx.root, base: "/", mode: "production", command: "build", environments: { nitro: {} } },
          env: { command: "build", mode: "production" },
          environment: { name: "client", mode: "build" },
          ojStartMode: true,
          ojBundleHost: true,
        }),
      ],
      env: { OJ_CACHE_ROOT: fx.root, ...env },
      cwd: fx.root,
    });
    try {
      await host.initSignal();
      host.child.stdin.write(JSON.stringify({ id: 1, hook: "buildApp", args: [built] }) + "\n");
      const request = await host.nextFrame();
      assert.equal(request.method, "buildEnvironments", JSON.stringify(request));
      log("buildEnvironments");
      return await host
        .send({ id: 2, hook: "rpcReply", args: [String(request.rpc), JSON.stringify({ result: null })] })
        .then(async (reply) => (reply.id === 1 ? reply : host.nextFrame()));
    } finally {
      host.close();
    }
  };
  const log = (line) => fs.appendFileSync(path.join(fx.root, "events.log"), line + "\n");
  try {
    const ok = await run({});
    assert.equal(ok.error, undefined, JSON.stringify(ok));
    assert.deepEqual(events(fx), [
      "pre this.environment=true built=false",
      "buildEnvironments",
      "post built=true,true outDir=/out",
      'nitro: oj build builds the client and ssr environments; the "nitro" environment a plugin builds is not supported yet',
    ]);
    fs.rmSync(path.join(fx.root, "events.log"));
    const failed = await run({ OJ_TEST_THROW: "1" });
    assert.match(String(failed.error), /refused/, JSON.stringify(failed));
  } finally {
    fx.cleanup();
  }
});

// Vite's bundleWorkerEntry: a worker bundle's plugins are `worker.plugins`
// alone (the option called for a fresh set), its resolved config `isWorker`
// with the bundle chain; the app's own plugins are not there. The worker's
// container lives in the app's realm, so the two sets share module state as
// they do under Vite, and a closed container answers no more hooks.
const viteSrc = path.join(repo, "e2e/fixtures/start-app/node_modules/vite");
test(
  "a worker bundle's container runs worker.plugins alone, beside the app's",
  { skip: !fs.existsSync(viteSrc) && "fixture vite not installed" },
  async () => {
    const fx = tmpProject({ prefix: "oj-worker-host-" });
    fs.symlinkSync(viteSrc, path.join(fx.root, "node_modules", "vite"), "dir");
    fx.write(
      "vite.config.mjs",
      `${LOG}
     let calls = 0;
     const shared = [];
     export default {
       plugins: [{
         name: "app",
         buildStart() { log("app buildStart " + this.environment.name); },
         closeBundle() { log("app closeBundle shared=" + shared.join(",")); },
       }],
       worker: {
         plugins: () => {
           calls++;
           return [[{
             name: "worker",
             configResolved(c) { log("worker configResolved isWorker=" + c.isWorker + " chain=" + c.bundleChain.join(",") + " calls=" + calls); },
             buildStart() { log("worker buildStart " + this.environment.name); shared.push("worker-" + calls); },
           }]];
         },
       },
     };\n`,
    );
    const host = rpcSidecar("plugin-host.mjs", {
      args: [
        path.join(fx.root, "vite.config.mjs"),
        JSON.stringify({
          pluginsFormat: "vite",
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
    const call = async (id, hook, args, container) => {
      const r = await host.send({ id, hook, args, container });
      assert.equal(r.error, undefined, `${hook}: ${r.error}\n${host.stderr()}`);
      return r.result;
    };
    try {
      await host.initSignal();
      const worker = Number(await call(1, "openWorkerContainer", [JSON.stringify(["/app/src/w.ts"])]));
      assert.ok(worker > 0, `a worker container id: ${worker}`);
      await call(2, "buildStart", [], worker);
      await call(3, "closeBundle", [], worker);
      await call(4, "closeContainer", [], worker);
      await call(5, "buildStart", []);
      await call(6, "closeBundle", []);
      assert.deepEqual(events(fx), [
        "worker configResolved isWorker=true chain=/app/src/w.ts calls=1",
        "worker buildStart client",
        "app buildStart client",
        "app closeBundle shared=worker-1",
      ]);
      const closed = await host.send({ id: 7, hook: "buildStart", args: [], container: worker });
      assert.match(String(closed.error), /container \d+ is closed/, JSON.stringify(closed));
    } finally {
      host.close();
      fx.cleanup();
    }
  },
);

// A worker plugin whose config hook fails is skipped in a Start build, but
// that never excuses the app's own plugins: their buildApp failure still
// fails the build.
test(
  "a worker's skipped config hook leaves the app's buildApp failure fatal",
  { skip: !fs.existsSync(viteSrc) && "fixture vite not installed" },
  async () => {
    const fx = tmpProject({ prefix: "oj-worker-skip-" });
    fx.write(
      "vite.config.mjs",
      `export default {
         plugins: [{ name: "app", buildApp: { order: "post", handler() { throw new Error("app refused"); } } }],
         worker: { plugins: () => [{ name: "worker", config() { throw new Error("worker config broke"); } }] },
       };\n`,
    );
    const host = rpcSidecar("plugin-host.mjs", {
      args: [
        path.join(fx.root, "vite.config.mjs"),
        JSON.stringify({
          pluginsFormat: "vite",
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
    try {
      await host.initSignal();
      const opened = await host.send({ id: 1, hook: "openWorkerContainer", args: [JSON.stringify(["/app/src/w.ts"])] });
      assert.equal(opened.error, undefined, JSON.stringify(opened));
      const built = JSON.stringify({ client: { outDir: "/out/client", input: "c.tsx" } });
      host.child.stdin.write(JSON.stringify({ id: 2, hook: "buildApp", args: [built] }) + "\n");
      const request = await host.nextFrame();
      assert.equal(request.method, "buildEnvironments", JSON.stringify(request));
      const reply = await host
        .send({ id: 3, hook: "rpcReply", args: [String(request.rpc), JSON.stringify({ result: null })] })
        .then(async (r) => (r.id === 2 ? r : host.nextFrame()));
      assert.match(String(reply.error), /\[plugin:app\] app refused/, JSON.stringify(reply));
    } finally {
      host.close();
      fx.cleanup();
    }
  },
);

// Each Vite environment resolves its own `vite:css-post`: CSS a worker
// plugin routes through it ships with the worker, not the app's stylesheet.
// The worker's own natively run plugins stay listed and drive its facts.
test(
  "a worker container's css-post and plugin list are its own",
  { skip: !fs.existsSync(viteSrc) && "fixture vite not installed" },
  async () => {
    const fx = tmpProject({ prefix: "oj-worker-css-" });
    // Each plugin hands CSS to the css-post its resolved config lists.
    fx.write(
      "vite.config.mjs",
      `import { appendFileSync } from "node:fs";
       const routes = (name) => {
         let resolved;
         return {
           name,
           configResolved(c) { resolved = c; },
           buildStart() {
             const post = resolved.plugins.find((p) => p.name === "vite:css-post");
             post.transform.handler("." + name + "{}", "/" + name + ".css");
             const listed = resolved.plugins.some((p) => p.name === "vite:react-refresh");
             appendFileSync(new URL("./events.log", import.meta.url), name + " lists react-refresh=" + listed + "\\n");
           },
         };
       };
       const refresh = { name: "vite:react-refresh" };
       export default {
         plugins: [routes("app")],
         worker: { plugins: () => [routes("worker"), refresh] },
       };\n`,
    );
    const host = rpcSidecar("plugin-host.mjs", {
      args: [
        path.join(fx.root, "vite.config.mjs"),
        JSON.stringify({
          pluginsFormat: "vite",
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
    const call = async (id, hook, args, container) => {
      const r = await host.send({ id, hook, args, container });
      assert.equal(r.error, undefined, `${hook}: ${r.error}\n${host.stderr()}`);
      return r.result;
    };
    try {
      await host.initSignal();
      const worker = Number(await call(1, "openWorkerContainer", [JSON.stringify(["/app/src/w.ts"])]));
      await call(2, "buildStart", [], worker);
      await call(3, "buildStart", []);
      assert.deepEqual(JSON.parse(await call(4, "getPluginCss", [])), [{ id: "/app.css", css: ".app{}" }]);
      assert.deepEqual(JSON.parse(await call(5, "getPluginCss", [], worker)), [
        { id: "/worker.css", css: ".worker{}" },
      ]);
      assert.deepEqual(events(fx), ["worker lists react-refresh=true", "app lists react-refresh=false"]);
      assert.equal(JSON.parse(await call(6, "startFacts", [], worker)).reactRefresh, true);
      assert.equal(JSON.parse(await call(7, "startFacts", [])).reactRefresh, false);
    } finally {
      host.close();
      fx.cleanup();
    }
  },
);
