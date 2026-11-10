// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

// The plugin host must pass a Vite-shaped HmrContext ({ file, timestamp, type,
// modules, read }) to handleHotUpdate and honor a returned filtered module list.
// Before, the ctx was only { file, timestamp } and a non-empty array was ignored.
test("plugin-host enriches the handleHotUpdate ctx; the modules returned as handed are no filter", async () => {
  const fx = tmpProject({ prefix: "oj-hmr-" });
  fx.write("a.tsx", "export const x = 1;\n");
  fx.write(
    "oj.plugins.mjs",
    `export default [{
      name: "hmr",
      async handleHotUpdate(ctx) {
        const content = await ctx.read();
        const ok =
          ctx.type === "update" &&
          typeof ctx.timestamp === "number" &&
          Array.isArray(ctx.modules) &&
          ctx.modules[0] && ctx.modules[0].url === "/a.tsx" &&
          ctx.modules[0].isSelfAccepting === true &&
          content.includes("export const x");
        // The modules as they came when the whole ctx was delivered; a wrong
        // ctx forces a full-reload, which the test detects as a failure.
        return ok ? ctx.modules : "full-reload";
      },
    }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const modules = JSON.stringify([{ url: "/a.tsx", isSelfAccepting: true, importers: ["/b.tsx"] }]);
    const res = await host.send({
      id: 1,
      hook: "handleHotUpdate",
      args: [path.join(fx.root, "a.tsx"), "123", "update", modules],
    });
    assert.equal(res.id, 1);
    // Vite's updateModules runs on the same modules as with no hook: the
    // change takes the default path (null), not a filter of its own.
    assert.equal(res.result, null, "the ctx was fully delivered, and the same modules are the default update");
  } finally {
    host.close();
    fx.cleanup();
  }
});

// A plugin returning an empty array suppresses HMR entirely (Vite: no modules
// to update). The host must surface that as "skip", not a filter or a reload.
test("plugin-host treats an empty handleHotUpdate array as a suppressed update", async () => {
  const fx = tmpProject({ prefix: "oj-hmr-" });
  fx.write("a.tsx", "export const x = 1;\n");
  fx.write(
    "oj.plugins.mjs",
    `export default [{
      name: "hmr-suppress",
      handleHotUpdate() {
        return [];
      },
    }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const res = await host.send({
      id: 1,
      hook: "handleHotUpdate",
      args: [path.join(fx.root, "a.tsx"), "123", "update", JSON.stringify([{ url: "/a.tsx", isSelfAccepting: true, importers: [] }])],
    });
    assert.equal(res.result, "skip", "empty array suppresses the update");
  } finally {
    host.close();
    fx.cleanup();
  }
});

// Vite folds the return across plugins: each returned array becomes the next
// plugin's ctx.modules and the FINAL set decides. An earlier empty array must
// not latch a suppression that a later plugin overrides.
test("plugin-host folds handleHotUpdate returns so the last plugin decides", async () => {
  const fx = tmpProject({ prefix: "oj-hmr-" });
  fx.write("a.tsx", "export const x = 1;\n");
  fx.write(
    "oj.plugins.mjs",
    `export default [
      { name: "empty-first", handleHotUpdate() { return []; } },
      { name: "narrow-last", handleHotUpdate() { return [{ url: "/a.tsx" }]; } },
    ];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const res = await host.send({
      id: 1,
      hook: "handleHotUpdate",
      args: [path.join(fx.root, "a.tsx"), "123", "update", "[]"],
    });
    const parsed = JSON.parse(res.result);
    assert.equal(parsed.action, "filter", "a later non-empty return overrides an earlier empty one");
    assert.deepEqual(parsed.modules, ["/a.tsx"]);
  } finally {
    host.close();
    fx.cleanup();
  }
});

// The mirror of the fold: a later empty array overrides an earlier filter and
// suppresses the update.
test("plugin-host lets a later empty handleHotUpdate return suppress an earlier filter", async () => {
  const fx = tmpProject({ prefix: "oj-hmr-" });
  fx.write("a.tsx", "export const x = 1;\n");
  fx.write(
    "oj.plugins.mjs",
    `export default [
      { name: "narrow-first", handleHotUpdate() { return [{ url: "/a.tsx" }]; } },
      { name: "empty-last", handleHotUpdate() { return []; } },
    ];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const res = await host.send({
      id: 1,
      hook: "handleHotUpdate",
      args: [path.join(fx.root, "a.tsx"), "123", "update", JSON.stringify([{ url: "/a.tsx", isSelfAccepting: true, importers: [] }])],
    });
    assert.equal(res.result, "skip", "the last plugin's empty array wins");
  } finally {
    host.close();
    fx.cleanup();
  }
});

// What the hooks leave is compared with what they were handed, as sets: the
// same modules are the update Vite's updateModules would run with no hook, so
// the host answers null and the default path decides. That path is also where
// a file oj's graph holds no module for is decided (a `<link>` stylesheet, an
// html page): handed none, a hook returning them (or `[]`) changes nothing.
test("plugin-host answers null when the hooks leave the modules they were handed", async () => {
  const fx = tmpProject({ prefix: "oj-hmr-" });
  fx.write("a.tsx", "export const x = 1;\n");
  fx.write(
    "oj.plugins.mjs",
    `export default [{
      name: "by-file",
      handleHotUpdate({ file, modules }) {
        if (file.endsWith("copy.tsx")) return [...modules].reverse();
        if (file.endsWith("more.tsx")) return [...modules, { url: "/extra.tsx" }];
        if (file.endsWith("fewer.tsx")) return modules.slice(0, 1);
        return modules;
      },
    }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  const two = JSON.stringify([{ url: "/a.tsx" }, { url: "/b.tsx" }]);
  const ask = async (id, file, modules) =>
    (await host.send({ id, hook: "handleHotUpdate", args: [path.join(fx.root, file), "123", "update", modules] })).result;
  try {
    assert.equal(await ask(1, "index.html", "[]"), null, "handed none, returned none: the default path");
    assert.equal(await ask(2, "a.tsx", two), null, "the same array");
    assert.equal(await ask(3, "copy.tsx", two), null, "the same modules in another array and order");
    assert.deepEqual(JSON.parse(await ask(4, "more.tsx", two)), {
      action: "filter",
      modules: ["/a.tsx", "/b.tsx", "/extra.tsx"],
    });
    assert.deepEqual(JSON.parse(await ask(5, "fewer.tsx", two)), { action: "filter", modules: ["/a.tsx"] });
  } finally {
    host.close();
    fx.cleanup();
  }
});
