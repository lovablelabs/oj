// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Under Start a throwing config hook is skipped per plugin, and the host's
// config falls back from the app's failed Vite resolve to its own: the hooks
// that ran inside that resolve keep what they did (in-place edits included),
// and none of them, the throwing one either, runs a second time. The file's
// own values stay under the hooks' (Vite merges the loaded file under the
// inline config before any hook runs): its `css` reaches configResolved.

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { repo, rpcSidecar, tmpProject } from "./harness.mjs";

const viteSrc = path.join(repo, "e2e/fixtures/start-app/node_modules/vite");

test(
  "a throwing config hook under Start keeps the earlier hooks' edits and runs once",
  { skip: !fs.existsSync(viteSrc) && "fixture vite not installed" },
  async () => {
    const fx = tmpProject({ prefix: "oj-config-fallback-" });
    fs.symlinkSync(viteSrc, path.join(fx.root, "node_modules", "vite"), "dir");
    const dump = path.join(fx.root, "dump.json");
    const calls = path.join(fx.root, "calls.log");
    fx.write(
      "vite.config.mjs",
      `import fs from "node:fs";
export default { css: { devSourcemap: true }, plugins: [
  { name: "edits-in-place", config(c) { c.define ??= {}; c.define.__A__ = "1"; c.resolve ??= {}; c.resolve.dedupe = ["a-dedupe"]; } },
  { name: "thrower", config() { fs.appendFileSync(${JSON.stringify(calls)}, "thrower\\n"); throw new Error("boom"); } },
  { name: "dump", configResolved(c) { fs.writeFileSync(${JSON.stringify(dump)}, JSON.stringify({ define: c.define, dedupe: c.resolve?.dedupe, css: c.css })); } },
] };\n`,
    );
    const host = rpcSidecar("plugin-host.mjs", {
      args: [
        path.join(fx.root, "vite.config.mjs"),
        JSON.stringify({
          pluginsFormat: "vite",
          config: { root: fx.root, base: "/", mode: "development", command: "serve" },
          env: { command: "serve", mode: "development" },
          environment: { name: "client", mode: "dev" },
          ojStartMode: true,
        }),
      ],
      env: { OJ_CACHE_ROOT: fx.root },
      cwd: fx.root,
    });
    try {
      await host.initSignal();
      const seen = JSON.parse(fs.readFileSync(dump, "utf8"));
      assert.equal(seen.define?.__A__, "1", JSON.stringify(seen));
      assert.deepEqual(seen.dedupe, ["a-dedupe"], JSON.stringify(seen));
      assert.equal(fs.readFileSync(calls, "utf8"), "thrower\n", "the throwing hook ran once");
      assert.equal(seen.css?.devSourcemap, true, JSON.stringify(seen));
      assert.equal(seen.css?.transformer, "postcss", JSON.stringify(seen));
    } finally {
      host.close();
      fx.cleanup();
    }
  },
);

// oj passes a vite.config app's `environments` to the host as read from the
// file, and the host's config already holds the file (the app's resolve, or
// the file itself when that failed). Merged a second time, every
// per-environment array would carry each entry twice; Vite's own resolve
// sees the file once.
for (const throws of [false, true]) {
  test(
    `the file's per-environment arrays appear once ${throws ? "when the resolve fails" : "over the app's resolve"}`,
    { skip: !fs.existsSync(viteSrc) && "fixture vite not installed" },
    async () => {
      const fx = tmpProject({ prefix: "oj-config-envs-" });
      fs.symlinkSync(viteSrc, path.join(fx.root, "node_modules", "vite"), "dir");
      const dump = path.join(fx.root, "dump.json");
      const environments = { ssr: { resolve: { conditions: ["node-only"], noExternal: ["pkg-a"] } } };
      fx.write(
        "vite.config.mjs",
        `import fs from "node:fs";
export default { environments: ${JSON.stringify(environments)}, plugins: [
  ${throws ? `{ name: "thrower", config() { throw new Error("boom"); } },` : ""}
  { name: "dump", configResolved(c) { fs.writeFileSync(${JSON.stringify(dump)}, JSON.stringify(c.environments?.ssr?.resolve)); } },
] };\n`,
      );
      const host = rpcSidecar("plugin-host.mjs", {
        args: [
          path.join(fx.root, "vite.config.mjs"),
          JSON.stringify({
            pluginsFormat: "vite",
            // As oj sends it: the file's environments, verbatim.
            config: { root: fx.root, base: "/", mode: "development", command: "serve", environments },
            env: { command: "serve", mode: "development" },
            environment: { name: "client", mode: "dev" },
            ojStartMode: true,
          }),
        ],
        env: { OJ_CACHE_ROOT: fx.root },
        cwd: fx.root,
      });
      try {
        await host.initSignal();
        const ssr = JSON.parse(fs.readFileSync(dump, "utf8"));
        assert.deepEqual(ssr.conditions, ["node-only"], JSON.stringify(ssr));
        assert.deepEqual(ssr.noExternal, ["pkg-a"], JSON.stringify(ssr));
      } finally {
        host.close();
        fx.cleanup();
      }
    },
  );
}
