// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import vm from "node:vm";
import { asset, repo } from "./harness.mjs";

// hmr-runtime.js as rolldown inlines it: after its DevRuntime (stubbed here,
// a store with no HMR decisions over a fixed module graph) in a page's global
// scope. `log` records what the page says to the server and its console errors.
const GRAPH = { "src/app.tsx": [], "src/a.tsx": ["src/app.tsx"] };
const STUB = `class DevRuntime {
  constructor(id) { this.clientId = id; }
  isExecuted(id) { return id in GRAPH; }
  getImporters(id) { return GRAPH[id] || []; }
  hasFactory(id) { return id in GRAPH; }
  removeModuleCache(id) { this.hooks.onModuleCacheRemoval(id); }
  initModule(id) { log.push("run " + id); }
  loadExports() { return {}; }
}`;

function loadRuntime(source = readFileSync(asset("start/hmr-runtime.js"), "utf8"), bus = {}) {
  const log = [];
  const page = {
    GRAPH,
    log,
    // Set by the entry's first import, before the runtime runs.
    __ojEngine: "engine",
    console: { debug() {}, error: (...m) => log.push("error " + m.join(" ")) },
    location: { reload: () => log.push("location.reload") },
    document: { querySelectorAll: () => [] },
    __ojStartHmr: {
      send: (msg) => (log.push(msg), true),
      post: (msg) => log.push(msg),
      ...bus,
    },
  };
  page.globalThis = page;
  vm.createContext(page);
  vm.runInContext(`${STUB}\n${source}`, page);
  const runtime = page.__rolldown_runtime__;
  runtime.log = log;
  return runtime;
}

const settle = (runtime) => runtime.hmr.queue;

test("the runtime's id starts with the tag of the engine that made its bundle", () => {
  assert.match(loadRuntime().clientId, /^engine\.[0-9a-z]+$/);
});

test("a re-run module's hot.on listeners replace its old ones, as Vite's HMRContext", async () => {
  const runtime = loadRuntime();
  const calls = [];
  const first = runtime.hooks.createModuleHotContext("src/a.tsx");
  first.on("vite:beforeUpdate", () => calls.push("first"));
  const other = runtime.hooks.createModuleHotContext("src/b.tsx");
  other.on("vite:beforeUpdate", () => calls.push("other"));

  // The update re-runs src/a.tsx: a new context, the same listener again.
  const second = runtime.hooks.createModuleHotContext("src/a.tsx");
  const again = () => calls.push("second");
  second.on("vite:beforeUpdate", again);
  await runtime.hmr.notify("vite:beforeUpdate", {});
  assert.deepEqual(calls.sort(), ["other", "second"]);

  // off() drops it from both the event's list and the module's own.
  calls.length = 0;
  second.off("vite:beforeUpdate", again);
  runtime.hooks.createModuleHotContext("src/a.tsx");
  await runtime.hmr.notify("vite:beforeUpdate", {});
  assert.deepEqual(calls, ["other"]);
});

test("vite:beforeUpdate fires before the patch is imported, as Vite's bundled dev client", async () => {
  const runtime = loadRuntime();
  const app = runtime.hooks.createModuleHotContext("src/app.tsx");
  app.accept();
  app.on("vite:beforeUpdate", (p) => runtime.log.push("beforeUpdate " + p.updates[0].path));
  // The vm has no module loader, so the patch import fails: the listener ran first.
  runtime.ojApply({ url: "/@oj-start/hmr/hmr_patch_0.js", seq: 1, changedIds: ["src/a.tsx"] });
  await settle(runtime);
  assert.deepEqual(
    runtime.log.map((l) => (typeof l === "string" ? l : l.type)),
    ["beforeUpdate src/app.tsx", "oj:reload-needed"],
  );
});

test("a reload from beforeApply (an overlay up before the first update) skips the patch", async () => {
  const runtime = loadRuntime(undefined, { beforeApply: () => "reload" });
  runtime.hooks.createModuleHotContext("src/app.tsx").accept();
  runtime.ojApply({ url: "/@oj-start/hmr/hmr_patch_0.js", seq: 1, changedIds: ["src/a.tsx"] });
  await settle(runtime);
  assert.deepEqual(runtime.log, [], "no import, no second reload request");
});

test("hot.invalidate notifies vite:invalidate and re-runs the importers in place", async () => {
  const runtime = loadRuntime();
  const app = runtime.hooks.createModuleHotContext("src/app.tsx");
  app.accept();
  const events = [];
  app.on("vite:invalidate", (p) => events.push(["invalidate", { ...p }]));
  app.on("vite:beforeUpdate", (p) => events.push(["before", p.updates[0]]));
  app.on("vite:afterUpdate", (p) => events.push(["after", p.updates[0].path]));
  runtime.hooks.createModuleHotContext("src/a.tsx").invalidate("exports changed");
  await settle(runtime);
  assert.deepEqual(events[0], [
    "invalidate",
    { path: "src/a.tsx", message: "exports changed", firstInvalidatedBy: "src/a.tsx" },
  ]);
  assert.equal(events[1][0], "before");
  assert.equal(events[1][1].path, "src/app.tsx");
  assert.equal(events[1][1].firstInvalidatedBy, "src/a.tsx");
  assert.equal(events[1][1].isWithinCircularImport, false);
  assert.deepEqual(events[2], ["after", "src/app.tsx"]);
  assert.deepEqual(runtime.log, ["run src/app.tsx"], "re-run in place, no reload");
});

test("an update that throws is logged and the page keeps running, as Vite's warnFailedUpdate", async () => {
  const runtime = loadRuntime();
  runtime.hooks.createModuleHotContext("src/app.tsx").accept(() => {
    throw new Error("accept handler broke");
  });
  runtime.hooks.createModuleHotContext("src/a.tsx").invalidate();
  await settle(runtime);
  assert.ok(runtime.log.some((l) => typeof l === "string" && l.includes("accept handler broke")));
  assert.ok(runtime.log.some((l) => typeof l === "string" && l.includes("Failed to reload src/a.tsx")));
  assert.ok(!runtime.log.some((l) => l === "location.reload" || l.type === "oj:reload-needed"));
});

test("hot.send posts a custom event for the dev server's ws listeners", () => {
  const runtime = loadRuntime();
  runtime.hooks.createModuleHotContext("src/a.tsx").send("app:ping", { n: 1 });
  assert.deepEqual(JSON.parse(JSON.stringify(runtime.log)), [{ type: "custom", event: "app:ping", data: { n: 1 } }]);
});

// The page's `import()` of a module asks the dev server for it by id
// (rolldown's `requestLazy`, which keeps the request per module and hands
// each caller a promise derived from it).
const LAZY_STUB = `class DevRuntime {
  constructor(id) { this.clientId = id; this.moduleCache = new Map(); this.factories = new Map(); this.lazyRequests = new Map(); }
  registerFactory(id, fn) { this.factories.set(id, fn); }
  removeModuleCache(id) { this.moduleCache.delete(id); this.lazyRequests.delete(id); }
  initModule(id) { if (!this.moduleCache.has(id)) this.factories.get(id)(id); return this.moduleCache.get(id); }
  requestLazy(id, proxy) {
    let pending = this.lazyRequests.get(id);
    if (!pending) {
      pending = Promise.resolve().then(() => this.loadLazyChunk(proxy)).then(() => this.initModule(id));
      this.lazyRequests.set(id, pending);
    }
    return pending.then((exports) => exports);
  }
}`;

function lazyRuntime() {
  const source = readFileSync(asset("start/hmr-runtime.js"), "utf8").replaceAll("__OJ_DEV_BASE__", '"/app"');
  const page = {
    console: { debug() {}, error() {}, warn() {} },
    __ojStartHmr: { send: () => true },
    setTimeout,
    clearTimeout,
    Date,
  };
  page.globalThis = page;
  vm.createContext(page);
  vm.runInContext(`${LAZY_STUB}\n${source}`, page);
  return page.__rolldown_runtime__;
}

test("an import() the server did not answer is asked for again, each request under a url of its own", async () => {
  const runtime = lazyRuntime();
  const proxy = "/abs/src/note.ts?rolldown-lazy=1";
  const first = runtime.lazyChunkUrl(proxy);
  assert.ok(first.startsWith("/app/@oj-start/hmr/lazy?id=%2Fabs%2Fsrc%2Fnote.ts%3Frolldown-lazy%3D1&clientId="), first);
  assert.notEqual(runtime.lazyChunkUrl(proxy), first, "the browser keeps a module, failed or not, by url");

  // The vm has no module loader: the import of the chunk fails, as the
  // browser's does when the server does not answer. Two call sites await it.
  const [a, b] = [runtime.requestLazy("src/note.ts", proxy), runtime.requestLazy("src/note.ts", proxy)];
  await assert.rejects(a);
  await assert.rejects(b);
  await new Promise((r) => setTimeout(r, 0));
  let asked = 0;
  runtime.loadLazyChunk = async () => {
    asked++;
    runtime.registerFactory("src/note.ts", (id) => runtime.moduleCache.set(id, { note: "fixed" }));
  };
  // The next import asks again; the first failure's handlers, both run by
  // now, do not drop the request that replaced it.
  const again = runtime.requestLazy("src/note.ts", proxy);
  await new Promise((r) => setTimeout(r, 0));
  assert.deepEqual(await again, { note: "fixed" });
  await runtime.requestLazy("src/note.ts", proxy);
  assert.equal(asked, 1);
});

test("an import() settles when a module with top-level await has finished, and rejects if it threw", async () => {
  const runtime = lazyRuntime();
  let finish;
  runtime.loadLazyChunk = async () => {
    runtime.registerFactory("src/tla.ts", async (id) => {
      const exports = {};
      runtime.moduleCache.set(id, exports);
      await new Promise((r) => (finish = r));
      exports.done = true;
    });
  };
  let settled = false;
  const imported = runtime
    .requestLazy("src/tla.ts", "/abs/src/tla.ts?rolldown-lazy=1")
    .then((m) => ((settled = true), m));
  await new Promise((r) => setTimeout(r, 5));
  assert.equal(settled, false, "the import settled before the module finished evaluating");
  finish();
  assert.deepEqual(await imported, { done: true });

  runtime.loadLazyChunk = async () => {
    runtime.registerFactory("src/throws.ts", async (id) => {
      runtime.moduleCache.set(id, {});
      await null;
      throw new Error("tla threw");
    });
  };
  const proxy = "/abs/src/throws.ts?rolldown-lazy=1";
  await assert.rejects(runtime.requestLazy("src/throws.ts", proxy), /tla threw/);
  await assert.rejects(runtime.requestLazy("src/throws.ts", proxy), /tla threw/, "as a module that threw stays failed");
});

test("a module compiled after a hot update waits for the page to have applied it", async () => {
  const runtime = lazyRuntime();
  let applied = false;
  const waited = runtime.ojPatched(2).then(() => (applied = runtime.hmr.applied));
  await new Promise((r) => setTimeout(r, 30));
  assert.equal(applied, false, "it did not wait for update 2");
  // Woken by the updates as they are applied, not by a timer of its own.
  runtime.hmr.applied = 1;
  runtime.hmr.wake();
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(applied, false, "update 1 is not update 2");
  // Taken in is not applied.
  runtime.hmr.lastSeq = 2;
  runtime.hmr.wake();
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(applied, false, "update 2 is counted, not applied");
  runtime.hmr.applied = 2;
  runtime.hmr.wake();
  assert.equal(await waited, 2);
  // Nothing sent: nothing to wait for.
  await lazyRuntime().ojPatched(0);
});

test("a page that is about to reload, or that the update never reaches, does not hold the import", async () => {
  const reloading = lazyRuntime();
  const waited = reloading.ojPatched(5);
  reloading.hmr.requestReload("a sequence gap");
  await waited;

  // The bound, on a clock the test turns.
  const warned = [];
  const source = readFileSync(asset("start/hmr-runtime.js"), "utf8").replaceAll("__OJ_DEV_BASE__", '""');
  const timers = [];
  const page = {
    console: { debug() {}, error() {}, warn: (m) => warned.push(m) },
    __ojStartHmr: { send: () => true },
    setTimeout: (fn, ms, arg) => timers.push(() => fn(arg)),
    clearTimeout() {},
    Date,
  };
  page.globalThis = page;
  vm.createContext(page);
  vm.runInContext(`${LAZY_STUB}\n${source}`, page);
  const late = page.__rolldown_runtime__.ojPatched(3);
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(timers.length, 1);
  timers[0]();
  await late;
  assert.match(warned[0], /hot update 3 did not reach this page/);
});

// While the editor's gate holds a change, the server asks the page whether it
// already runs the held modules a compiled chunk carries.
const HELD_STUB = `class DevRuntime {
  constructor(id) {
    this.clientId = id; this.moduleCache = new Map(); this.factories = new Map(); this.factoryStamps = new Map();
    this.lazyRequests = new Map(); this.staticImports = new Map(); this.rowStamps = new Map();
  }
  registerFactory(id, fn, stamp = 0) {
    if (stamp < (this.factoryStamps.get(id) ?? 0)) return;
    this.factoryStamps.set(id, stamp);
    this.factories.set(id, fn);
  }
  registerGraph(delta) {
    for (let i = 0; i < delta.localCount; i++) {
      const id = delta.ids[i];
      if (delta.stamps) this.rowStamps.set(id, delta.stamps[i] ?? 0);
      this.staticImports.set(id, { edges: delta.edges[i].map((j) => delta.ids[j]) });
    }
  }
}`;

function heldRuntime() {
  const source = readFileSync(asset("start/hmr-runtime.js"), "utf8").replaceAll("__OJ_DEV_BASE__", '""');
  const page = { console: { debug() {}, error() {} }, __ojStartHmr: {}, queueMicrotask, setTimeout, Date };
  page.globalThis = page;
  vm.createContext(page);
  vm.runInContext(`${HELD_STUB}\n${source}`, page);
  return page.__rolldown_runtime__;
}

test("a chunk carrying held modules is taken only by a page that runs them all; one the server lost waits for the flush", async () => {
  const runtime = heldRuntime();
  const asked = [];
  runtime.ojImport = async (url) => {
    asked.push(url);
    if (url.includes("lazy_held_gone")) throw new TypeError("Failed to fetch dynamically imported module");
  };
  const proxy = "/abs/src/lazy.ts?rolldown-lazy=1";

  // The page never ran one of them: it asks again, to wait for the flush.
  runtime.moduleCache.set("src/util.ts", {});
  await runtime.ojLazyWhileHeld(["src/util.ts", "src/other.ts"], "/@oj-start/hmr/lazy_held_0.js", proxy);
  assert.equal(asked.length, 1);
  assert.match(asked[0], /^\/@oj-start\/hmr\/lazy\?id=.*&held=1$/);

  // It runs them all: it takes the chunk.
  asked.length = 0;
  runtime.moduleCache.set("src/other.ts", {});
  await runtime.ojLazyWhileHeld(["src/util.ts", "src/other.ts"], "/@oj-start/hmr/lazy_held_1.js", proxy);
  assert.deepEqual(asked, ["/@oj-start/hmr/lazy_held_1.js"]);

  // The kept chunk is gone: the module is asked for once more, to be
  // answered after the flush (not for another chunk to take, which could
  // be gone in turn, without end).
  asked.length = 0;
  await runtime.ojLazyWhileHeld(["src/util.ts"], "/@oj-start/hmr/lazy_held_gone.js", proxy);
  assert.equal(asked.length, 2);
  assert.match(asked[1], /^\/@oj-start\/hmr\/lazy\?id=.*&held=1$/);
});

test("a taken chunk leaves the held modules' factories, graph rows and stamps as the page has them", async () => {
  const runtime = heldRuntime();
  const patch = () => "the patch's factory";
  // The flush came while the chunk was on its way: its patch is applied.
  runtime.registerFactory("src/util.ts", patch, 7);
  runtime.registerGraph({ ids: ["src/util.ts", "src/dep-new.ts"], localCount: 1, edges: [[1]], stamps: { 0: 7 } });

  // The chunk, compiled before the flush, as the server brackets it.
  runtime.ojSkipHeld(["src/util.ts"]);
  runtime.registerGraph({
    ids: ["src/lazy.ts", "src/util.ts", "src/dep-old.ts"],
    localCount: 2,
    edges: [[1], [2]],
    stamps: { 0: 3, 1: 9 },
  });
  runtime.registerFactory("src/lazy.ts", () => "lazy");
  runtime.registerFactory("src/util.ts", () => "the held factory", 9);

  assert.equal(runtime.factories.get("src/util.ts")("src/util.ts"), patch());
  assert.equal(runtime.factoryStamps.get("src/util.ts"), 7);
  assert.deepEqual(runtime.staticImports.get("src/util.ts").edges, ["src/dep-new.ts"]);
  assert.equal(runtime.rowStamps.get("src/util.ts"), 7);
  // What the chunk carries besides is taken, its edge to the held module too.
  assert.ok(runtime.factories.has("src/lazy.ts"));
  assert.deepEqual(runtime.staticImports.get("src/lazy.ts").edges, ["src/util.ts"]);
  assert.equal(runtime.rowStamps.get("src/lazy.ts"), 3);

  // Only for that chunk's evaluation.
  await new Promise((r) => setTimeout(r, 0));
  runtime.registerFactory("src/util.ts", () => "a later patch", 10);
  assert.equal(runtime.factoryStamps.get("src/util.ts"), 10);
});

// rolldown's own DevRuntime (the file its hmr plugin inlines ahead of
// hmr-runtime.js), from the cargo registry at the version Cargo.lock pins:
// what `requestLazy` does with a factory already in the page is rolldown's.
function rolldownRuntimeFile() {
  const lock = readFileSync(join(repo, "Cargo.lock"), "utf8");
  const version = /name = "rolldown_plugin_hmr"\nversion = "([^"]+)"/.exec(lock)?.[1];
  const home = process.env.CARGO_HOME || join(homedir(), ".cargo");
  const src = join(home, "registry", "src");
  for (const index of existsSync(src) ? readdirSync(src) : []) {
    const file = join(src, index, `rolldown_plugin_hmr-${version}`, "src/runtime/runtime-extra-dev-common.js");
    if (existsSync(file)) return file;
  }
  return null;
}

function loadRealRuntime(file) {
  const common = readFileSync(file, "utf8").replaceAll("export class", "class");
  const source = readFileSync(asset("start/hmr-runtime.js"), "utf8")
    .replaceAll("__OJ_DEV_BASE__", '""')
    .replaceAll("import(/* @vite-ignore */", "__import(");
  const log = [];
  const urls = new Map();
  const page = {
    log,
    console: { debug() {}, warn() {}, error: (...m) => log.push("error " + m.join(" ")) },
    location: { reload: () => log.push("location.reload") },
    setTimeout,
    clearTimeout,
    queueMicrotask,
    __toESM: (x) => x,
    __toCommonJS: (x) => x,
    __exportAll: () => {},
    __reExport: () => {},
    __ojStartHmr: { send: (msg) => (log.push(msg), true), post: (msg) => log.push(msg) },
    __import: async (url) => {
      const key = [...urls.keys()].find((k) => url.startsWith(k));
      if (!key) throw new TypeError("Failed to fetch dynamically imported module: " + url);
      log.push("fetch " + key);
      return urls.get(key)();
    },
  };
  page.globalThis = page;
  vm.createContext(page);
  vm.runInContext(`${common}\n${source}`, page);
  return { runtime: page.__rolldown_runtime__, log, urls };
}

const realRuntime = rolldownRuntimeFile();

// A page on a route that reads `label`; `about` (behind an import(), never
// run here) reads it too. The patch for an edit of `label` carries `about`
// as it is then. `about` is then edited alone, and imported.
function editedAfterItsImport({ runtime, urls }, ran) {
  runtime.registerGraph({
    ids: ["index", "label", "about", "root"],
    localCount: 4,
    edges: [[1], [], [1], [0]],
    dynamicEdges: { 3: [2] },
  });
  runtime.registerModule("root");
  runtime.registerModule("index");
  runtime.registerModule("label", { exports: { v: 1 } });
  runtime.createModuleHotContext("index").accept();
  urls.set("/patch-1", () => {
    runtime.registerGraph({
      ids: ["about", "index", "label"],
      localCount: 3,
      edges: [[2], [2], []],
      stamps: { 0: 1, 1: 1, 2: 1 },
    });
    runtime.registerFactory(
      "about",
      (id) => (runtime.registerModule(id, { exports: { about: "v1" } }), ran.push("about v1")),
      1,
    );
    runtime.registerFactory(
      "index",
      (id) => (runtime.registerModule(id), runtime.createModuleHotContext(id).accept()),
      1,
    );
    runtime.registerFactory("label", (id) => runtime.registerModule(id, { exports: { v: 2 } }), 1);
  });
  const about2 = () => {
    runtime.registerGraph({ ids: ["about", "label"], localCount: 1, edges: [[1]], stamps: { 0: 2 } });
    runtime.registerFactory(
      "about",
      (id) => (runtime.registerModule(id, { exports: { about: "v2" } }), ran.push("about v2")),
      2,
    );
  };
  return about2;
}

test(
  "a module edited after a patch carried its old code is imported as edited (rolldown's runtime)",
  { skip: realRuntime || process.env.CI ? false : "rolldown_plugin_hmr is not in the cargo registry" },
  async () => {
    assert.ok(realRuntime, "rolldown_plugin_hmr's runtime file is in the cargo registry");
    const page = loadRealRuntime(realRuntime);
    const { runtime, urls, log } = page;
    const ran = [];
    const about2 = editedAfterItsImport(page, ran);
    urls.set("/patch-2", about2);
    runtime.ojApply({ url: "/patch-1", seq: 1, changedIds: ["label"] });
    await runtime.hmr.queue;
    runtime.ojApply({ url: "/patch-2", seq: 2, changedIds: ["about"] });
    await runtime.hmr.queue;
    assert.deepEqual(ran, [], "nothing ran the module the page has not imported");
    await runtime.requestLazy("about", "/abs/about?rolldown-lazy=1");
    assert.deepEqual(ran, ["about v2"]);
    assert.ok(
      !log.includes("location.reload") && !log.some((l) => l?.type === "oj:reload-needed"),
      JSON.stringify(log),
    );
  },
);

test(
  "a patch for a module the page has not run that cannot be fetched leaves the module to be asked for (rolldown's runtime)",
  { skip: realRuntime || process.env.CI ? false : "rolldown_plugin_hmr is not in the cargo registry" },
  async () => {
    assert.ok(realRuntime, "rolldown_plugin_hmr's runtime file is in the cargo registry");
    const page = loadRealRuntime(realRuntime);
    const { runtime, urls, log } = page;
    const ran = [];
    const about2 = editedAfterItsImport(page, ran);
    // The second patch is gone; the server answers the import with the module as it is.
    urls.set("/@oj-start/hmr/lazy", about2);
    runtime.ojApply({ url: "/patch-1", seq: 1, changedIds: ["label"] });
    await runtime.hmr.queue;
    runtime.ojApply({ url: "/patch-2", seq: 2, changedIds: ["about"] });
    await runtime.hmr.queue;
    await runtime.requestLazy("about", "/abs/about?rolldown-lazy=1");
    assert.deepEqual(ran, ["about v2"]);
    assert.ok(log.includes("fetch /@oj-start/hmr/lazy"), JSON.stringify(log));
    assert.ok(
      !log.includes("location.reload") && !log.some((l) => l?.type === "oj:reload-needed"),
      JSON.stringify(log),
    );
  },
);

test(
  "a listener of an update that awaits a first import() does not hold the update (rolldown's runtime)",
  { skip: realRuntime || process.env.CI ? false : "rolldown_plugin_hmr is not in the cargo registry" },
  async () => {
    assert.ok(realRuntime, "rolldown_plugin_hmr's runtime file is in the cargo registry");
    const { runtime, urls, log } = loadRealRuntime(realRuntime);
    runtime.registerGraph({ ids: ["app", "a"], localCount: 2, edges: [[1], []] });
    runtime.registerModule("app");
    runtime.registerModule("a");
    const app = runtime.createModuleHotContext("app");
    app.accept();
    const done = [];
    app.on("vite:beforeUpdate", async () => {
      await runtime.requestLazy("x", "/abs/x?rolldown-lazy=1");
      done.push("before");
    });
    runtime.createModuleHotContext("other").on("my:event", async () => {
      await runtime.requestLazy("y", "/abs/y?rolldown-lazy=1");
      done.push("custom");
    });
    // The server's answers name the last update it sent this page.
    let sent = 0;
    urls.set("/@oj-start/hmr/lazy?id=%2Fabs%2Fx", async () => {
      await runtime.ojPatched(sent);
      runtime.registerFactory("x", (id) => runtime.registerModule(id, { exports: {} }));
    });
    urls.set("/@oj-start/hmr/lazy?id=%2Fabs%2Fy", async () => {
      await runtime.ojPatched(sent);
      runtime.registerFactory("y", (id) => runtime.registerModule(id, { exports: {} }));
    });
    urls.set("/patch", () => {
      runtime.registerFactory("a", (id) => runtime.registerModule(id), sent);
      runtime.registerFactory(
        "app",
        (id) => (
          runtime.registerModule(id),
          runtime.initModule("a"),
          runtime.createModuleHotContext(id).accept(),
          log.push("app re-ran")
        ),
        sent,
      );
    });
    const within = (promise, what) =>
      Promise.race([promise, new Promise((_, no) => setTimeout(() => no(new Error(what + " never settled")), 2000))]);

    sent = 1;
    runtime.ojApply({ url: "/patch", seq: 1, changedIds: ["a"] });
    await within(runtime.hmr.queue, "the update whose listener imports");
    assert.deepEqual(done, ["before"]);
    assert.equal(log.filter((l) => l === "app re-ran").length, 1);

    // A custom event's listener, with update 2 sent and not yet taken in
    // (its frame is behind the event in the socket's queue).
    sent = 2;
    await within(runtime.hmr.notify("my:event", {}), "the custom event's listener");
    assert.deepEqual(done, ["before", "custom"]);
    runtime.ojApply({ url: "/patch", seq: 2, changedIds: ["a"] });
    await within(runtime.hmr.queue, "the next update");
    assert.equal(runtime.hmr.applied, 2);
    assert.equal(log.filter((l) => l === "app re-ran").length, 2, JSON.stringify(log));
  },
);
