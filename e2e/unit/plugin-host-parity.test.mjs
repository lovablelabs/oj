// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Plugin-host behaviors matched to Vite's plugin container / config resolution:
// ConfigEnv shape, apply(config), this.meta.viteVersion, warn with the plugin
// name, native plugins listed in config.plugins, applyToEnvironment returning
// plugins, per-hook `order` on transform / resolveId / load, resolveId options
// and result fidelity, load meta, the enriched this.environment, ws connection
// events, the transformIndexHtml ctx and error path, buildEnd(error) and an
// idempotent closeBundle.

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

function spawnHost(fx, initial = {}) {
  return rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ config: { root: fx.root }, ...initial })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
}

// The plugin file under test exposes what it observed through a `probe.js`
// transform that returns the captured state as JSON.
async function probe(host, fx) {
  const res = await host.send({ id: 99, hook: "transform", args: ["", path.join(fx.root, "probe.js")] });
  if (res.error) throw new Error(res.error);
  return JSON.parse(JSON.parse(res.result).code);
}

test("ConfigEnv carries isSsrBuild / isPreview and apply() sees the merged config with mode", async () => {
  const fx = tmpProject({ prefix: "oj-parity-" });
  fx.write(
    "oj.plugins.mjs",
    `const seen = {};
     export default [{
       name: "env-probe",
       apply(config, env) { seen.apply = { env, mode: config.mode, root: config.root }; return true; },
       config(_c, env) { seen.configEnv = env; },
       transform(code, id) { return id.endsWith("probe.js") ? JSON.stringify(seen) : null; },
     }];\n`,
  );
  const host = spawnHost(fx, { env: { command: "build", mode: "production" }, environment: { name: "ssr", mode: "build" } });
  try {
    const seen = await probe(host, fx);
    assert.deepEqual(seen.apply.env, { command: "build", mode: "production", isSsrBuild: true, isPreview: false });
    assert.deepEqual(seen.configEnv, { command: "build", mode: "production", isSsrBuild: true, isPreview: false });
    assert.equal(seen.apply.mode, "production", "apply(config) carries mode like Vite's { ...config, mode }");
    assert.equal(seen.apply.root, fx.root, "apply(config) is the merged config, not an empty shape");
  } finally {
    host.close();
    fx.cleanup();
  }
});

// Vite's NODE_ENV rule in the host, synthesized-config path included: the
// default follows the COMMAND, never the mode, is pre-set before any plugin
// or config code evaluates (as resolveConfig sets it before
// loadConfigFromFile), and isProduction derives from process.env.NODE_ENV —
// a custom `--mode staging` build is still a production build; serve stays
// development. An environment that already names NODE_ENV wins.
test("NODE_ENV follows the command under a custom mode and isProduction tracks it", async () => {
  const plugin = `const seen = { moduleScope: process.env.NODE_ENV };
     export default [{
       name: "nodeenv-probe",
       config() { seen.configHook = process.env.NODE_ENV; },
       configResolved(config) { seen.isProduction = config.isProduction; },
       transform(code, id) { return id.endsWith("probe.js") ? JSON.stringify(seen) : null; },
     }];\n`;
  const run = async (command, hostEnv = {}) => {
    const fx = tmpProject({ prefix: "oj-parity-nodeenv-" });
    fx.write("oj.plugins.mjs", plugin);
    const host = rpcSidecar("plugin-host.mjs", {
      args: [
        path.join(fx.root, "oj.plugins.mjs"),
        JSON.stringify({ config: { root: fx.root }, env: { command, mode: "staging" } }),
      ],
      // NODE_ENV cleared so the host's own pre-set is what the plugin sees
      // (an empty value counts as unset, as in the extractor).
      env: { OJ_CACHE_ROOT: fx.root, NODE_ENV: "", ...hostEnv },
      cwd: fx.root,
    });
    try {
      return await probe(host, fx);
    } finally {
      host.close();
      fx.cleanup();
    }
  };
  const build = await run("build");
  assert.equal(build.moduleScope, "production", "build --mode staging pre-sets NODE_ENV=production at module scope");
  assert.equal(build.configHook, "production");
  assert.equal(build.isProduction, true, "a custom-mode build is a production build");
  const serve = await run("serve");
  assert.equal(serve.moduleScope, "development", "serve --mode staging stays development");
  assert.equal(serve.isProduction, false);
  const forced = await run("build", { NODE_ENV: "development" });
  assert.equal(forced.moduleScope, "development", "a set NODE_ENV is never overridden");
  assert.equal(forced.isProduction, false, "isProduction follows the environment's NODE_ENV, as in Vite");
});

test("a client dev host reports isSsrBuild false", async () => {
  const fx = tmpProject({ prefix: "oj-parity-" });
  fx.write(
    "oj.plugins.mjs",
    `let env;
     export default [{
       name: "env-probe",
       apply(_c, e) { env = e; return true; },
       transform(code, id) { return id.endsWith("probe.js") ? JSON.stringify(env) : null; },
     }];\n`,
  );
  const host = spawnHost(fx);
  try {
    assert.deepEqual(await probe(host, fx), { command: "serve", mode: "development", isSsrBuild: false, isPreview: false });
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("this.meta.viteVersion is set and this.warn names the plugin", async () => {
  const fx = tmpProject({ prefix: "oj-parity-" });
  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "warner",
       transform(code, id) {
         if (!id.endsWith("probe.js")) return null;
         this.warn("careful now");
         return JSON.stringify({ viteVersion: this.meta.viteVersion, rollupVersion: this.meta.rollupVersion, pluginName: this.pluginName });
       },
     }];\n`,
  );
  const host = spawnHost(fx);
  try {
    const seen = await probe(host, fx);
    assert.match(seen.viteVersion, /^\d+\.\d+/, "viteVersion looks like a version");
    assert.equal(typeof seen.rollupVersion, "string");
    assert.equal(seen.pluginName, "warner");
    await host.waitStderr(/warning: careful now\n\s+Plugin: warner/);
    assert.match(host.stderr(), /warning: careful now\n\s+Plugin: warner/, "the warning names its plugin");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("natively reimplemented react plugins stay listed in config.plugins without running hooks", async () => {
  const fx = tmpProject({ prefix: "oj-parity-" });
  fx.write(
    "oj.plugins.mjs",
    `let names = [];
     let resolvedNames = [];
     export default [
       { name: "vite:react-babel", transform() { throw new Error("must not run"); } },
       {
         name: "prober",
         config(c) { names = c.plugins.map((p) => p.name); },
         configResolved(c) { resolvedNames = c.plugins.map((p) => p.name); },
         transform(code, id) { return id.endsWith("probe.js") ? JSON.stringify({ names, resolvedNames }) : null; },
       },
     ];\n`,
  );
  const host = spawnHost(fx);
  try {
    const seen = await probe(host, fx);
    assert.ok(seen.names.includes("vite:react-babel"), `config() sees the native plugin: ${seen.names}`);
    assert.ok(seen.resolvedNames.includes("vite:react-babel"), `configResolved sees it too: ${seen.resolvedNames}`);
    const count = await host.send({ id: 2, hook: "getPluginCount", args: [] });
    assert.equal(count.result, "1", "the native plugin is not an active hook runner");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("applyToEnvironment is awaited; a returned plugin list replaces the wrapper and a falsy result drops it", async () => {
  const fx = tmpProject({ prefix: "oj-parity-" });
  fx.write(
    "oj.plugins.mjs",
    `export default [
       {
         name: "wrapper",
         async applyToEnvironment() {
           return [
             { name: "inner", transform(code, id) { return id.endsWith("probe.js") ? JSON.stringify({ inner: true }) : null; }, configureServer() {} },
             null,
           ];
         },
         transform() { throw new Error("the wrapper's own hooks must be replaced"); },
       },
       { name: "gone", async applyToEnvironment() { return false; }, transform() { throw new Error("gone must be dropped"); } },
     ];\n`,
  );
  const host = spawnHost(fx);
  try {
    assert.deepEqual(await probe(host, fx), { inner: true });
    await host.waitStderr(/Plugin "inner" defines Vite-specific hooks \(configureServer\)/);
    assert.match(host.stderr(), /Plugin "inner" defines Vite-specific hooks \(configureServer\)/, "config-phase hooks on returned plugins are warned about");
    const count = await host.send({ id: 2, hook: "getPluginCount", args: [] });
    assert.equal(count.result, "1");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("per-hook order sorts transform, resolveId and load like Vite's getSortedPluginsByHook", async () => {
  const fx = tmpProject({ prefix: "oj-parity-" });
  fx.write(
    "oj.plugins.mjs",
    `export default [
       { name: "post", transform: { order: "post", handler(code) { return code + "P"; } },
         resolveId: { order: "post", handler(s) { return s === "virt" ? "post-won" : null; } },
         load: { order: "post", handler(id) { return id === "virt-load" ? "post-load" : null; } } },
       { name: "normal", transform(code) { return code + "N"; },
         resolveId(s) { return s === "virt" ? "normal-won" : null; },
         load(id) { return id === "virt-load" ? "normal-load" : null; } },
       { name: "pre", transform: { order: "pre", handler(code) { return code + "R"; } },
         resolveId: { order: "pre", handler(s) { return s === "virt" ? "pre-won" : null; } },
         load: { order: "pre", handler(id) { return id === "virt-load" ? "pre-load" : null; } } },
     ];\n`,
  );
  const host = spawnHost(fx);
  try {
    const t = await host.send({ id: 1, hook: "transform", args: ["", path.join(fx.root, "a.js")] });
    assert.equal(JSON.parse(t.result).code, "RNP", "pre runs first, post last, regardless of array position");
    const r = await host.send({ id: 2, hook: "resolveId", args: ["virt", ""] });
    assert.equal(r.result, "pre-won");
    const l = await host.send({ id: 3, hook: "load", args: ["virt-load"] });
    assert.equal(l.result, "pre-load");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("resolveId gets Vite's options, results keep external/meta, load meta reaches getModuleInfo", async () => {
  const fx = tmpProject({ prefix: "oj-parity-" });
  fx.write(
    "oj.plugins.mjs",
    `const seen = {};
     export default [
       {
         name: "resolver",
         resolveId(source, importer, options) {
           if (source !== "sibling") return null;
           seen.options = options;
           return { id: "\\0sibling", external: true, meta: { fromResolve: 1 }, moduleSideEffects: false };
         },
         load(id, options) {
           if (id !== "loaded.js") return null;
           seen.loadOptions = options;
           return { code: "export const l = 1;", map: null, meta: { fromLoad: 2 } };
         },
       },
       {
         name: "asker",
         async transform(code, id) {
           if (id === "loaded.js") {
             seen.loadedMeta = this.getModuleInfo(id)?.meta;
             return null;
           }
           if (!id.endsWith("probe.js")) return null;
           seen.resolved = await this.resolve("sibling", id, { isEntry: true, attributes: { type: "json" } });
           return JSON.stringify(seen);
         },
       },
     ];\n`,
  );
  const host = spawnHost(fx);
  try {
    await host.send({ id: 1, hook: "load", args: ["loaded.js"] });
    await host.send({ id: 2, hook: "transform", args: ["export const l = 1;", "loaded.js"] });
    const seen = await probe(host, fx);
    assert.deepEqual(seen.options, { attributes: { type: "json" }, custom: {}, isEntry: true, ssr: false, scan: false });
    assert.deepEqual(seen.resolved, { id: "\0sibling", external: true, meta: { fromResolve: 1 }, moduleSideEffects: false });
    assert.deepEqual(seen.loadOptions, { ssr: false });
    assert.deepEqual(seen.loadedMeta, { fromLoad: 2 }, "a load result's meta is on the module info in transform");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("this.environment carries moduleGraph/logger/hot and moduleParsed info has Rollup's fields", async () => {
  const fx = tmpProject({ prefix: "oj-parity-" });
  fx.write(
    "oj.plugins.mjs",
    `let parsedKeys = [];
     export default [{
       name: "env-shape",
       moduleParsed(info) { parsedKeys = Object.keys(info); },
       transform(code, id) {
         if (!id.endsWith("probe.js")) return null;
         const e = this.environment;
         return JSON.stringify({
           name: e.name,
           consumer: e.config.consumer,
           moduleGraph: typeof e.moduleGraph?.getModuleById,
           logger: typeof e.logger?.warn,
           hot: typeof e.hot?.send,
           topLevel: typeof e.getTopLevelConfig,
           plugins: Array.isArray(e.plugins),
           parsedKeys,
         });
       },
     }];\n`,
  );
  const host = spawnHost(fx);
  try {
    await host.send({ id: 1, hook: "transform", args: ["let a;", path.join(fx.root, "first.js")] });
    const seen = await probe(host, fx);
    assert.equal(seen.name, "client");
    assert.equal(seen.consumer, "client");
    assert.equal(seen.moduleGraph, "function");
    assert.equal(seen.logger, "function");
    assert.equal(seen.hot, "function");
    assert.equal(seen.topLevel, "function");
    assert.equal(seen.plugins, true);
    for (const k of ["id", "code", "meta", "importers", "importedIds", "isEntry", "moduleSideEffects"]) {
      assert.ok(seen.parsedKeys.includes(k), `moduleParsed info has ${k}: ${seen.parsedKeys}`);
    }
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("server.ws.on('connection') fires per client and the socket's send relays through oj", async () => {
  const fx = tmpProject({ prefix: "oj-parity-" });
  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "greeter",
       configureServer(server) {
         server.ws.on("connection", (socket, req) => {
           socket.send(JSON.stringify({ type: "custom", event: "server:hello", data: { url: req.url } }));
         });
       },
     }];\n`,
  );
  const host = spawnHost(fx);
  try {
    const frames = [await host.send({ id: 1, hook: "wsConnection", args: [] })];
    frames.push(await host.nextFrame());
    const ws = frames.find((f) => f.ojWs);
    const reply = frames.find((f) => f.id === 1);
    assert.ok(reply, "the RPC is answered");
    assert.deepEqual(ws.ojWs, { event: "server:hello", data: { url: "/" } });
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("transformIndexHtml gets the page ctx (path, filename, server) and a throwing hook fails the request", async () => {
  const fx = tmpProject({ prefix: "oj-parity-" });
  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "html-ctx",
       transformIndexHtml(html, ctx) {
         if (html.includes("THROW")) throw new Error("no html for you");
         return [{ tag: "meta", attrs: { name: "ctx", content: [ctx.path, ctx.filename, typeof ctx.server?.ws?.send, ctx.originalUrl].join("|") } }];
       },
     }];\n`,
  );
  const host = spawnHost(fx);
  try {
    const ctx = JSON.stringify({ path: "/sub/page.html", filename: "/abs/sub/page.html", originalUrl: "/sub/" });
    const ok = await host.send({ id: 1, hook: "transformIndexHtml", args: ["<html><head></head><body></body></html>", ctx] });
    assert.match(ok.result, /content="\/sub\/page.html\|\/abs\/sub\/page.html\|function\|\/sub\/"/);
    const bad = await host.send({ id: 2, hook: "transformIndexHtml", args: ["<html>THROW</html>", ctx] });
    assert.equal(bad.result, undefined);
    assert.match(bad.error, /\[plugin:html-ctx\] no html for you/, "the error names the plugin and propagates");
    assert.match(bad.error, /\/abs\/sub\/page.html/, "and the page file");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("buildEnd receives the build error and closeBundle runs once per build", async () => {
  const fx = tmpProject({ prefix: "oj-parity-" });
  fx.write(
    "oj.plugins.mjs",
    `const seen = { closeBundle: 0, buildEnd: [] };
     export default [{
       name: "lifecycle",
       buildEnd(err) { seen.buildEnd.push(err ? err.message : null); },
       closeBundle() { seen.closeBundle++; },
       transform(code, id) { return id.endsWith("probe.js") ? JSON.stringify(seen) : null; },
     }];\n`,
  );
  const host = spawnHost(fx, { env: { command: "build", mode: "production" } });
  try {
    await host.send({ id: 1, hook: "buildEnd", args: ["build failed: boom"] });
    await host.send({ id: 2, hook: "closeBundle", args: [] });
    await host.send({ id: 3, hook: "closeBundle", args: [] });
    const seen = await probe(host, fx);
    assert.deepEqual(seen.buildEnd, ["build failed: boom"]);
    assert.equal(seen.closeBundle, 1);
  } finally {
    host.close();
    fx.cleanup();
  }
});

// The host's config merge is Vite's mergeConfigRecursively slice
// (mergeConfigLite, twin of vite-extract.mjs's): a hook returning `key: null`
// must not clobber a set value, and `true` on either side of
// ssr `noExternal`/`external` wins over lists — later hooks (and the resolved
// config plugins read) see the true, never a `[list, true]` concat.
test("config-hook merges skip null overrides and apply the ssr noExternal true-wins rule", async () => {
  const fx = tmpProject({ prefix: "oj-parity-merge-" });
  fx.write(
    "oj.plugins.mjs",
    `const seen = {};
     export default [
       {
         name: "gives-true",
         config: () => ({ base: null, ssr: { noExternal: true }, resolve: { dedupe: ["react"] } }),
       },
       {
         name: "gives-list-after-true",
         config: () => ({ ssr: { noExternal: ["late-list"] } }),
       },
       {
         name: "observer",
         config(conf) {
           seen.base = conf.base;
           seen.noExternal = conf.ssr && conf.ssr.noExternal;
           seen.dedupe = conf.resolve && conf.resolve.dedupe;
         },
         configResolved(config) {
           seen.resolvedNoExternal = config.ssr && config.ssr.noExternal;
         },
         transform(code, id) { return id.endsWith("probe.js") ? JSON.stringify(seen) : null; },
       },
     ];\n`,
  );
  const host = spawnHost(fx, { config: { root: fx.root, base: "/keep/", ssr: { noExternal: ["from-config"] } } });
  try {
    const seen = await probe(host, fx);
    assert.equal(seen.base, "/keep/", "a null override must not clobber a set value");
    assert.equal(seen.noExternal, true, "true wins over the list (Vite's ssr noExternal special case)");
    assert.equal(seen.resolvedNoExternal, true, "and a later hook's list does not demote it");
    assert.deepEqual(seen.dedupe, ["react"], "ordinary keys still merge");
  } finally {
    host.close();
    fx.cleanup();
  }
});
