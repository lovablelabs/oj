// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { asset } from "./harness.mjs";

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
