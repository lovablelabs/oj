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
// with the bundle chain; the app's own plugins are not there.
const viteSrc = path.join(repo, "e2e/fixtures/start-app/node_modules/vite");
test(
  "a worker bundle host runs worker.plugins alone",
  { skip: !fs.existsSync(viteSrc) && "fixture vite not installed" },
  async () => {
    const fx = tmpProject({ prefix: "oj-worker-host-" });
    fs.symlinkSync(viteSrc, path.join(fx.root, "node_modules", "vite"), "dir");
    fx.write(
      "vite.config.mjs",
      `${LOG}
     let calls = 0;
     export default {
       plugins: [{ name: "app", buildStart() { log("app buildStart"); } }],
       worker: {
         plugins: () => {
           calls++;
           return [[{
             name: "worker",
             configResolved(c) { log("worker configResolved isWorker=" + c.isWorker + " chain=" + c.bundleChain.join(",") + " calls=" + calls); },
             buildStart() { log("worker buildStart " + this.environment.name); },
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
          ojWorkerBundle: { chain: ["/app/src/w.ts"] },
        }),
      ],
      env: { OJ_CACHE_ROOT: fx.root },
      cwd: fx.root,
    });
    try {
      const r = await host.send({ id: 1, hook: "buildStart", args: [] });
      assert.equal(r.error, undefined, `${r.error}\n${host.stderr()}`);
      assert.deepEqual(events(fx), [
        "worker configResolved isWorker=true chain=/app/src/w.ts calls=1",
        "worker buildStart client",
      ]);
    } finally {
      host.close();
      fx.cleanup();
    }
  },
);
