// SPDX-License-Identifier: MIT

import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { __test as bundle } from "../../crates/oj_server/src/assets/start/bundle-client.mjs";
import {
  __test as bridge,
  createPluginContainer,
  loadPluginContainer,
} from "../../crates/oj_server/src/assets/start/vite-plugin-bridge.mjs";

test("Solid client selects initialized router transforms, not the Start compiler or duplicate JSX compiler", async () => {
  const calls = [];
  let initialized = false;
  const plugins = [
    {
      name: "tanstack:router-generator",
      enforce: "pre",
      configResolved() {
        initialized = true;
        calls.push("generator");
      },
    },
    ...["reference", "virtual", "shared"].map((kind) => ({
      name: `tanstack-router:code-splitter:compile-${kind}-file`,
      enforce: "pre",
      configResolved() {
        assert.ok(initialized);
        calls.push(`init:${kind}`);
      },
      buildStart() {
        calls.push(`start:${kind}`);
      },
      transform: {
        filter: {
          id: kind === "reference" ? /\.tsx$/ : new RegExp(`tsr-${kind === "virtual" ? "split" : "shared"}`),
        },
        handler(code, id) {
          assert.ok(initialized);
          calls.push(id);
          return `${kind}:${code}`;
        },
      },
    })),
    {
      name: "tanstack-start-core::server-fn:client",
      configResolved() {
        assert.fail("Start compiler initialized");
      },
      buildStart() {
        assert.fail("Start compiler started");
      },
      transform() {
        assert.fail("Start compiler ran");
      },
    },
    {
      name: "solid",
      transform() {
        assert.fail("duplicate JSX compilation");
      },
    },
    {
      name: "app-transform",
      transform(code) {
        return `${code}:user`;
      },
    },
  ];
  const container = createPluginContainer({}, plugins, {
    routerTransforms: true,
  });
  await container.buildStart();
  await container.buildStart();
  assert.deepEqual(calls.splice(0), [
    "generator",
    "init:reference",
    "init:virtual",
    "init:shared",
    "start:reference",
    "start:virtual",
    "start:shared",
  ]);
  assert.equal(await container.transformUserCode("jsx", "/src/routes/index.tsx"), "reference:jsx:user");
  assert.equal(
    await container.transformUserCode("jsx", "/src/routes/index.tsx?tsr-split=component"),
    "virtual:jsx:user",
  );
  assert.equal(await container.transformUserCode("jsx", "/src/routes/index.tsx?tsr-shared"), "shared:jsx:user");
  assert.deepEqual(calls, [
    "/src/routes/index.tsx",
    "/src/routes/index.tsx?tsr-split=component",
    "/src/routes/index.tsx?tsr-shared",
  ]);
});

test("router opt-in is bounded to serve/client; default React transforms stay unchanged", async () => {
  for (const opts of [
    {},
    { routerTransforms: true, command: "build" },
    { routerTransforms: true, environment: "ssr" },
  ]) {
    const container = createPluginContainer(
      {},
      [
        {
          name: "tanstack-router:code-splitter:compile-reference-file",
          configResolved() {
            assert.fail("router initialized");
          },
          transform() {
            assert.fail("router transformed");
          },
        },
        {
          name: "app-transform",
          transform() {
            return "user";
          },
        },
      ],
      opts,
    );
    assert.equal(await container.transformUserCode("jsx", "/route.tsx"), "user");
  }
});

test("client hook includes queried route modules, keeps plugin query identity and clean RPC identity", async () => {
  const ids = [];
  const rpcIds = [];
  const plugin = bundle.clientTransformPlugin(
    {
      async transformUserCode(code, id) {
        ids.push(id);
        return `${code}:router`;
      },
    },
    (code, id) => {
      rpcIds.push(id);
      return `${code}:rpc`;
    },
  );
  for (const id of ["/src/route.tsx", "/src/route.tsx?tsr-split=component", "/src/route.tsx?tsr-shared"]) {
    assert.ok(bridge.idAllowed(plugin.transform.filter, id));
    assert.equal(await plugin.transform.handler("jsx", id), "jsx:router:rpc");
  }
  assert.deepEqual(ids, ["/src/route.tsx", "/src/route.tsx?tsr-split=component", "/src/route.tsx?tsr-shared"]);
  assert.deepEqual(rpcIds, ["/src/route.tsx", "/src/route.tsx", "/src/route.tsx"]);
  for (const id of [
    "/node_modules/pkg/route.tsx?tsr-split=component",
    "\0virtual.tsx?tsr-split=component",
    "/src/style.css?raw",
  ]) {
    assert.ok(!bridge.idAllowed(plugin.transform.filter, id));
    assert.equal(await plugin.transform.handler("jsx", id), null);
  }
});

test("router config initializes the client-tree load filter without enabling Start compilation", async () => {
  const root = mkdtempSync(join(tmpdir(), "oj-router-client-"));
  try {
    const vite = join(root, "node_modules/vite");
    mkdirSync(vite, { recursive: true });
    writeFileSync(join(root, "package.json"), '{"name":"router-client-test"}');
    writeFileSync(join(root, "vite.config.mjs"), "export default {};\n");
    writeFileSync(join(vite, "package.json"), '{"name":"vite","type":"module","main":"./index.mjs"}');
    writeFileSync(
      join(vite, "index.mjs"),
      `
      export async function loadConfigFromFile({ mode }) {
        let config;
        let generatorReady = false;
        const clientTree = {
          name: "tanstack-start:route-tree-client-plugin",
          enforce: "pre",
          applyToEnvironment: (env) => env.name === "client",
          config() {
            if (!config) throw new Error("Start config missing");
            if (mode === "tree-config-error") throw new Error("tree filter initialization failed");
            clientTree.load.filter = { id: { include: new RegExp(config.root + "/src/routeTree.gen.ts") } };
          },
          load: {
            filter: {},
            // Like Start's real plugin, the handler relies entirely on its filter.
            handler() {
              if (!generatorReady) throw new Error("Generator instance not initialized");
              if (mode === "tree-load-error") throw new Error("Crawling result not available");
              return { code: "export const routeTree = 'client';", map: null };
            },
          },
        };
        return { config: { plugins: [
          { name: "tanstack-start-core:config", config(c) { config = c; },
            configResolved() { throw new Error("Start lifecycle must stay disabled"); } },
          clientTree,
          { name: "tanstack:router-generator", configResolved(c) {
            if (!config || config.root !== c.root) throw new Error("Start config missing");
            generatorReady = true;
          } },
          { name: "tanstack-router:code-splitter:compile-reference-file", transform() {
            if (!config) throw new Error("Start config missing");
            return config.root;
          } },
          { name: "tanstack-start-core:compiler", config() { throw new Error("compiler config ran"); },
            transform() { throw new Error("compiler ran"); } },
        ] } };
      }
    `,
    );
    const client = await loadPluginContainer(root, { routerTransforms: true });
    assert.equal(await client.transformUserCode("jsx", "/route.tsx"), root);
    assert.equal(await client.load(join(root, "src/routes/index.tsx")), null);
    assert.equal(await client.load(join(root, "src/components/Hero.tsx")), null);
    assert.equal(await client.load(join(root, "src/routeTree.gen.ts")), "export const routeTree = 'client';");
    const react = await loadPluginContainer(root);
    assert.equal(await react.transformUserCode("jsx", "/route.tsx"), null);
    assert.equal(await react.load(join(root, "src/routeTree.gen.ts")), null);
    for (const opts of [{ command: "build" }, { environment: "ssr" }]) {
      const inactive = await loadPluginContainer(root, { routerTransforms: true, ...opts });
      assert.equal(await inactive.load(join(root, "src/routeTree.gen.ts")), null);
    }
    await assert.rejects(
      loadPluginContainer(root, { routerTransforms: true, mode: "tree-config-error" }),
      /\[plugin:tanstack-start:route-tree-client-plugin\].*tree filter initialization failed/,
    );
    const failing = await loadPluginContainer(root, { routerTransforms: true, mode: "tree-load-error" });
    assert.equal(await failing.load(join(root, "src/routes/index.tsx")), null);
    await assert.rejects(
      failing.load(join(root, "src/routeTree.gen.ts")),
      /\[plugin:tanstack-start:route-tree-client-plugin\].*Crawling result not available/,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("required router lifecycle errors fail rather than silently dropping splitting", async () => {
  const container = createPluginContainer(
    {},
    [
      {
        name: "tanstack-router:code-splitter:compile-reference-file",
        configResolved() {
          throw new Error("missing router options");
        },
        transform() {
          return null;
        },
      },
    ],
    { routerTransforms: true },
  );
  await assert.rejects(
    container.transformUserCode("jsx", "/route.tsx"),
    /\[plugin:tanstack-router:code-splitter:compile-reference-file\].*missing router options/,
  );
});
