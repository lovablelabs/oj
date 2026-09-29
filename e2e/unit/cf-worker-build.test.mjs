// SPDX-License-Identifier: MIT

// The Cloudflare production build: when @cloudflare/vite-plugin is in the
// config, `vite build` bundles a Worker environment (declared by the plugin's
// `config` hook, named after the wrangler config or `viteEnvironment.name`) for
// workerd, and the plugin's own hooks emit wrangler.json. oj mirrors that from
// the environment the plugin declared; these tests pin the pieces the build
// script relies on, without the plugin or workerd installed.

import { test } from "node:test";
import assert from "node:assert/strict";
import { existsSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { repo } from "./harness.mjs";
import {
  cloudflareEnvironment,
  cloudflareWorkerPlugin,
  workerOutDir,
  CLOUDFLARE_WORKER_ENTRY,
} from "../../crates/oj_server/src/assets/start/cf-build.mjs";
import {
  createPluginContainer,
  loadPluginContainer,
} from "../../crates/oj_server/src/assets/start/vite-plugin-bridge.mjs";

// What the plugin's config + configEnvironment hooks leave in `config.environments`
// for a Worker named "my-worker" with nodejs_compat, mapped onto Vite's "ssr".
const workerOptions = {
  resolve: {
    noExternal: true,
    conditions: ["workerd", "worker", "module", "browser", "development|production"],
    builtins: ["cloudflare:workers", "cloudflare:sockets", "node:buffer", "buffer", "node:async_hooks"],
  },
  define: { "process.env.NODE_ENV": '"production"' },
  build: {
    target: "es2024",
    manifest: true,
    outDir: "dist/ssr",
    ssr: true,
    rolldownOptions: {
      input: { index: CLOUDFLARE_WORKER_ENTRY },
      platform: "neutral",
      plugins: [{ name: "esm-external-require" }],
    },
  },
};
const config = {
  build: { outDir: "dist" },
  environments: { ssr: workerOptions, client: { build: { outDir: "dist/client" } } },
};

test("cloudflareEnvironment reads the Worker environment the plugin declared", () => {
  assert.equal(cloudflareEnvironment(null), null);
  assert.equal(
    cloudflareEnvironment({ environments: { client: {}, ssr: { build: {} } } }),
    null,
    "a plain Vite config has no Worker environment",
  );
  const env = cloudflareEnvironment(config);
  assert.equal(env.name, "ssr");
  assert.deepEqual(
    env.conditions,
    ["workerd", "worker", "module", "browser"],
    "the mode condition is added by the build",
  );
  assert.equal(env.target, "es2024");
  assert.equal(env.nodejsCompat, true);
  assert.equal(env.outDir, "dist/ssr");
  assert.deepEqual(
    env.rolldownPlugins.map((p) => p.name),
    ["esm-external-require"],
  );
  // Several Workers: the entry Worker (the one writing the Vite manifest) wins.
  const multi = cloudflareEnvironment({
    environments: {
      aux_worker: { ...workerOptions, build: { ...workerOptions.build, manifest: false, outDir: "dist/aux_worker" } },
      my_worker: { ...workerOptions, build: { ...workerOptions.build, outDir: "dist/my_worker" } },
      client: {},
    },
  });
  assert.equal(multi.name, "my_worker");
  assert.equal(
    cloudflareEnvironment({
      environments: { w: { build: { rolldownOptions: { input: { index: CLOUDFLARE_WORKER_ENTRY } } } } },
    }).name,
    "w",
    "the virtual Worker entry alone identifies the environment",
  );
});

test("workerOutDir keeps the environment's directory under oj's output root", () => {
  const env = cloudflareEnvironment(config);
  assert.equal(workerOutDir(env, "/app", "/app/dist", config), "/app/dist/ssr");
  assert.equal(
    workerOutDir(env, "/app", "/app/out", config),
    "/app/out/ssr",
    "--out moves the root, the Worker dir follows",
  );
  const custom = {
    ...config,
    build: { outDir: "build" },
    environments: {
      ...config.environments,
      ssr: { ...workerOptions, build: { ...workerOptions.build, outDir: "elsewhere/worker" } },
    },
  };
  assert.equal(
    workerOutDir(cloudflareEnvironment(custom), "/app", "/app/build", custom),
    "/app/elsewhere/worker",
    "an outDir outside the root output stays where the plugin resolved it",
  );
});

test("the Worker plugin leaves cloudflare:* and the runtime's node built-ins external", async () => {
  const container = createPluginContainer(
    {},
    [
      {
        name: "polyfills",
        resolveId(id) {
          if (id === "node:fs") return "/polyfills/fs.mjs";
          if (id === "node:os") return { id: "node:os", external: true };
          return null;
        },
      },
    ],
    { command: "build", environment: "ssr", config: { root: "/app", environments: config.environments } },
  );
  const env = cloudflareEnvironment(config);
  const plugin = cloudflareWorkerPlugin({ container, env, serverEntry: "/oj/server-entry.tsx" });
  const resolve = (id) => plugin.resolveId.handler.call({}, id, "/app/src/main.ts");
  assert.deepEqual(await resolve("cloudflare:workers"), { id: "cloudflare:workers", external: true });
  assert.deepEqual(
    await resolve("node:async_hooks"),
    { id: "node:async_hooks", external: true },
    "a runtime built-in stays external",
  );
  assert.deepEqual(await resolve("buffer"), { id: "buffer", external: true }, "the bare form too");
  assert.equal(await resolve("node:fs"), "/polyfills/fs.mjs", "a plugin's polyfill answer is used as is");
  assert.deepEqual(
    await resolve("node:os"),
    { id: "node:os", external: true },
    "a plugin's external answer is kept external",
  );
  assert.equal(await resolve("node:child_process"), null, "an unhandled built-in falls through to the bundler");
  const filter = plugin.resolveId.filter.id.include;
  const matches = (id) => filter.some((re) => re.test(id));
  assert.ok(
    matches("node:path") &&
      matches("path") &&
      matches("unenv/node/fs") &&
      matches("@cloudflare/unenv-preset/node/console") &&
      matches("virtual:cloudflare/user-entry"),
  );
  assert.ok(!matches("react") && !matches("./local.js"));
});

test("the plugin swaps Start's default Worker entry for oj's server entry", async () => {
  const seen = [];
  const container = createPluginContainer(
    {},
    [
      {
        name: "vite-plugin-cloudflare:virtual-modules",
        async resolveId(id) {
          if (id !== "virtual:cloudflare/user-entry") return null;
          const main = await this.resolve("./worker/main.ts");
          seen.push(main);
          return main;
        },
      },
    ],
    { command: "build", environment: "ssr", config: { root: "/app" } },
  );
  const env = cloudflareEnvironment(config);
  const plugin = cloudflareWorkerPlugin({ container, env, serverEntry: "/oj/server-entry.tsx" });
  await assert.rejects(
    () => plugin.resolveId.handler.call({}, "virtual:cloudflare/user-entry"),
    /wrangler config `main`/,
    "an unresolvable main fails loud",
  );
  const swapped = cloudflareWorkerPlugin({
    container: createPluginContainer(
      {},
      [
        {
          name: "cf",
          resolveId: (id) =>
            id === "virtual:cloudflare/user-entry"
              ? "/app/node_modules/@tanstack/react-start/dist/default-entry/esm/server.js"
              : null,
        },
      ],
      { command: "build", environment: "ssr" },
    ),
    env,
    serverEntry: "/oj/server-entry.tsx",
  });
  assert.equal(await swapped.resolveId.handler.call({}, "virtual:cloudflare/user-entry"), "/oj/server-entry.tsx");
  const own = cloudflareWorkerPlugin({
    container: createPluginContainer(
      {},
      [{ name: "cf", resolveId: (id) => (id === "virtual:cloudflare/user-entry" ? "/app/server-entry.ts" : null) }],
      { command: "build", environment: "ssr" },
    ),
    env,
    serverEntry: "/oj/server-entry.tsx",
  });
  assert.equal(
    await own.resolveId.handler.call({}, "virtual:cloudflare/user-entry"),
    "/app/server-entry.ts",
    "an app's own main is kept",
  );
});

test("this.environment.config layers the environment's options over the top-level config", async () => {
  let seen;
  const container = createPluginContainer(
    {},
    [
      {
        name: "reader",
        load(id) {
          if (id !== "probe") return null;
          seen = {
            outDir: this.environment.config.build.outDir,
            consumer: this.environment.config.consumer,
            root: this.environment.config.root,
            publicDir: this.environment.config.publicDir,
            builtins: this.environment.config.resolve.builtins,
            define: this.environment.config.define,
            clientOut: this.environment.config.environments.client.build.outDir,
          };
          return "export default 1;";
        },
      },
    ],
    {
      command: "build",
      environment: "ssr",
      config: { root: "/app", define: { __TOP__: "1" }, environments: config.environments },
    },
  );
  await container.load("probe");
  assert.equal(seen.outDir, "dist/ssr");
  assert.equal(seen.consumer, "server");
  assert.equal(seen.root, "/app");
  assert.equal(seen.publicDir, "/app/public", "publicDir resolves to an absolute path, as Vite's does");
  assert.deepEqual(seen.builtins, workerOptions.resolve.builtins);
  assert.deepEqual(seen.define, { __TOP__: "1", "process.env.NODE_ENV": '"production"' });
  assert.equal(seen.clientOut, "dist/client");
  // Defaults when the config declares nothing for the environment.
  let plain;
  const bare = createPluginContainer(
    {},
    [
      {
        name: "r",
        load(id) {
          if (id === "p") {
            plain = this.environment.config;
            return "x";
          }
          return null;
        },
      },
    ],
    { command: "build", environment: "client", config: { root: "/app", publicDir: false } },
  );
  await bare.load("p");
  assert.equal(plain.build.outDir, "dist");
  assert.equal(plain.publicDir, "", "publicDir: false is the empty string");
});

test("resolveIdResult keeps `external`; this.resolve reaches the filesystem in a build", async () => {
  const app = mkdtempSync(join(tmpdir(), "oj-cf-resolve-"));
  writeFileSync(join(app, "package.json"), JSON.stringify({ name: "app", type: "module" }));
  writeFileSync(join(app, "real.mjs"), "export default 1;");
  try {
    const plugins = [
      { name: "ext", resolveId: (id) => (id === "node:os" ? { id: "node:os", external: true } : null) },
      {
        name: "asker",
        async resolveId(id) {
          return id === "ask" ? await this.resolve("./real.mjs") : null;
        },
      },
    ];
    const build = createPluginContainer({}, plugins, { command: "build", environment: "ssr", config: { root: app } });
    assert.deepEqual(await build.resolveIdResult("node:os"), { id: "node:os", external: true });
    assert.equal(await build.resolveId("node:os"), "node:os");
    assert.equal(await build.resolveId("ask"), join(app, "real.mjs"), "a real file resolves for a build plugin");
    assert.equal(await build.resolveId("missing"), null);
    const serve = createPluginContainer({}, plugins, { command: "serve", environment: "ssr", config: { root: app } });
    assert.equal(await serve.resolveId("ask"), null, "the dev loader resolves files itself");
  } finally {
    rmSync(app, { recursive: true, force: true });
  }
});

// configEnvironment needs the config file loaded through the app's Vite.
const fixture = join(repo, "e2e/fixtures/start-app");
const installed = existsSync(join(fixture, "node_modules/vite"));
(installed ? test : test.skip)("plugin configEnvironment partials merge into config.environments", async () => {
  const app = mkdtempSync(join(tmpdir(), "oj-cf-configenv-"));
  writeFileSync(join(app, "package.json"), JSON.stringify({ name: "app", type: "module" }));
  writeFileSync(
    join(app, "vite.config.mjs"),
    [
      "export default {",
      "  plugins: [",
      '    { name: "worker", config() { return { environments: { my_worker: { resolve: { builtins: ["cloudflare:workers"] }, build: { outDir: "dist/my_worker" } } } }; } },',
      '    { name: "compat", configEnvironment(name) { if (name === "my_worker") return { resolve: { builtins: ["node:buffer"] }, build: { rolldownOptions: { plugins: [{ name: "req" }] } } }; } },',
      '    { name: "client-only", configEnvironment(name, options, env) { return { define: { __ENV__: JSON.stringify(name + ":" + env.command) } }; } },',
      "  ],",
      "};",
    ].join("\n"),
  );
  symlinkSync(join(fixture, "node_modules"), join(app, "node_modules"), "dir");
  try {
    const container = await loadPluginContainer(app, { command: "build", environment: "my_worker" });
    const worker = container.config.environments.my_worker;
    assert.deepEqual(
      worker.resolve.builtins,
      ["cloudflare:workers", "node:buffer"],
      "arrays concatenate like Vite's mergeConfig",
    );
    assert.equal(worker.build.outDir, "dist/my_worker");
    assert.deepEqual(
      worker.build.rolldownOptions.plugins.map((p) => p.name),
      ["req"],
    );
    assert.equal(worker.define.__ENV__, '"my_worker:build"');
    assert.equal(
      container.config.environments.client.define.__ENV__,
      '"client:build"',
      "Vite's default environments get the hook too",
    );
    assert.equal(container.config.environments.ssr.define.__ENV__, '"ssr:build"');
    assert.equal(container.defines().__ENV__, '"my_worker:build"', "defines() reads the container's environment");
    const env = cloudflareEnvironment(container.config);
    assert.equal(env.name, "my_worker");
    assert.equal(env.nodejsCompat, true);
  } finally {
    rmSync(app, { recursive: true, force: true });
  }
});
