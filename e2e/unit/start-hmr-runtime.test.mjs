// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { asset } from "./harness.mjs";

// hmr-runtime.js as rolldown inlines it: after its DevRuntime (stubbed here,
// a store with no HMR decisions) in a page's global scope.
function loadRuntime(source = readFileSync(asset("start/hmr-runtime.js"), "utf8")) {
  const page = {
    console: { debug() {}, error() {} },
    location: { reload() {} },
    document: { querySelectorAll: () => [] },
  };
  page.globalThis = page;
  vm.createContext(page);
  vm.runInContext(`class DevRuntime { constructor(id) { this.clientId = id; } }\n${source}`, page);
  return page.__rolldown_runtime__;
}

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
