// SPDX-License-Identifier: MIT

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  __test,
  createPluginContainer,
  findConfig,
  loadPluginContainer,
} from "../../crates/oj_server/src/assets/start/vite-plugin-bridge.mjs";
import { mkdtempSync, rmSync, writeFileSync, mkdirSync, readFileSync, realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
const {
  matchOne,
  idAllowed,
  codeAllowed,
  byHook,
  applyMatches,
  ordered,
  hookHandler,
  hookFilter,
  ojReimplemented,
  envAllows,
} = __test;

test("matchOne: RegExp tests, string is a picomatch-style glob (Vite pluginFilter)", () => {
  assert.ok(matchOne(/\.mdx$/, "/a/b.mdx"));
  assert.ok(!matchOne(/\.mdx$/, "/a/b.tsx"));
  // A string is a glob: `**`-prefixed and absolute patterns are used as-is...
  assert.ok(matchOne("**/*.mdx", "/a/b.mdx"));
  assert.ok(!matchOne("**/*.mdx", "/a/b.tsx"));
  assert.ok(matchOne("/abs/dir/*.svg", "/abs/dir/x.svg"));
  // ...and a relative one is joined to the app root, so a bare substring no longer matches.
  const root = process.env.OJ_APP_ROOT ?? process.cwd();
  assert.ok(matchOne("src/**/*.tsx", root + "/src/pages/a.tsx"));
  assert.ok(!matchOne("src/**/*.tsx", "/elsewhere/src/pages/a.tsx"));
  assert.ok(!matchOne("virtual:", "virtual:foo"));
});

test("idAllowed: no filter allows everything", () => {
  assert.ok(idAllowed(undefined, "anything"));
  assert.ok(idAllowed(null, "anything"));
});

test("idAllowed: filter.id include (single + array)", () => {
  assert.ok(idAllowed({ id: /\.svg$/ }, "/icons/x.svg"));
  assert.ok(!idAllowed({ id: /\.svg$/ }, "/icons/x.png"));
  assert.ok(idAllowed({ id: [/\.svg$/, /\.png$/] }, "/icons/x.png"));
});

test("idAllowed: include/exclude object, exclude wins", () => {
  const f = { id: { include: /\/src\//, exclude: /\.test\./ } };
  assert.ok(idAllowed(f, "/src/app.ts"));
  assert.ok(!idAllowed(f, "/src/app.test.ts"));
  assert.ok(!idAllowed(f, "/lib/app.ts"));
});

test("idAllowed: a bare filter (not wrapped in .id) still applies", () => {
  assert.ok(idAllowed(/\.css$/, "/a.css"));
  assert.ok(!idAllowed(/\.css$/, "/a.js"));
});

test("applyMatches: no apply -> always; string vs command; function form", () => {
  assert.ok(applyMatches({}, "serve"));
  assert.ok(applyMatches({ apply: "serve" }, "serve"));
  assert.ok(!applyMatches({ apply: "build" }, "serve"));
  assert.ok(applyMatches({ apply: (_c, { command }) => command === "serve" }, "serve"));
  assert.ok(!applyMatches({ apply: (_c, { command }) => command === "build" }, "serve"));
  assert.ok(
    applyMatches(
      {
        apply: () => {
          throw new Error("x");
        },
      },
      "serve",
    ),
  );
});

test("ordered: pre first, normal next, post last (stable within a band)", () => {
  const names = ordered([
    { name: "n1" },
    { name: "post1", enforce: "post" },
    { name: "pre1", enforce: "pre" },
    { name: "n2" },
    { name: "pre2", enforce: "pre" },
  ]).map((p) => p.name);
  assert.deepEqual(names, ["pre1", "pre2", "n1", "n2", "post1"]);
});

test("ojReimplemented: skips React / Vite built-ins / TanStack; keeps app plugins", () => {
  for (const n of [
    "vite:react-babel",
    "vite:react-refresh",
    "vite:esbuild",
    "vite:import-glob",
    "tanstack-start-core::server-fn:client",
    "tanstack:router-generator",
    "tanstack-router:code-splitter:compile-reference-file",
    "@tanstack/react-start",
  ]) {
    assert.ok(ojReimplemented(n), `expected ${n} to be oj-reimplemented`);
  }
  for (const n of ["fixture-i18n", "ssr-stub-scopes", "transitive-preloads", "customer-mdx", "i18n-dev", ""]) {
    assert.ok(!ojReimplemented(n), `expected ${n} to be treated as an app plugin`);
  }
});

test("envAllows: applyToEnvironment gates per environment (the ssr-stub-scopes regression)", () => {
  assert.ok(envAllows({}, "client"));
  assert.ok(envAllows({}, "ssr"));
  const ssrOnly = { name: "ssr-stub-scopes", applyToEnvironment: (env) => env.name === "ssr" };
  assert.ok(!envAllows(ssrOnly, "client"), "ssr-only plugin must be skipped on client");
  assert.ok(envAllows(ssrOnly, "ssr"), "ssr-only plugin must run on ssr");
  const clientOnly = { applyToEnvironment: (env) => env.name === "client" };
  assert.ok(envAllows(clientOnly, "client"));
  assert.ok(!envAllows(clientOnly, "ssr"));
  assert.ok(
    envAllows(
      {
        applyToEnvironment: () => {
          throw new Error("x");
        },
      },
      "client",
    ),
  );
});

test("envAllows: passes env.config.consumer (@vitejs/plugin-react reads it)", () => {
  // The env handed to applyToEnvironment must carry config.consumer
  // ("client"/"server"); plugin-react reads it directly, so a bare {name}
  // env threw and killed the client bundle. consumer must match the env.
  const consumerOf = (env) => env.config.consumer;
  assert.equal(envAllows({ applyToEnvironment: consumerOf }, "client"), true);
  // a plugin-react-style gate: only the client consumer
  const clientConsumer = { applyToEnvironment: (env) => env.config.consumer === "client" };
  assert.ok(envAllows(clientConsumer, "client"));
  assert.ok(!envAllows(clientConsumer, "ssr"));
  // an async applyToEnvironment cannot be awaited in the sync filter, so a
  // thenable is treated as allowed rather than throwing on `!== false`.
  const asyncGate = { applyToEnvironment: async (env) => env.config.consumer === "client" };
  assert.ok(envAllows(asyncGate, "ssr"));
});

test("hookHandler / hookFilter: function form and object form", () => {
  const fn = () => 1;
  assert.equal(hookHandler(fn), fn);
  assert.equal(hookFilter(fn), undefined);

  const obj = { handler: fn, filter: { id: /\.mdx$/ }, order: "pre" };
  assert.equal(hookHandler(obj), fn);
  assert.deepEqual(hookFilter(obj), { id: /\.mdx$/ });

  assert.equal(hookHandler(undefined), null);
  assert.equal(hookHandler({ handler: "not-a-fn" }), null);
});

test("generateBundle honors environment consumer gates", async () => {
  const emitted = [];
  const plugin = {
    name: "synthetic-server-manifest",
    applyToEnvironment: (environment) => environment.config.consumer === "server",
    generateBundle() {
      this.emitFile({ type: "asset", fileName: "server-manifest.json", source: "{}" });
    },
  };

  await createPluginContainer({}, [plugin], { environment: "client" }).generateBundle((asset) => emitted.push(asset));
  assert.deepEqual(emitted, []);

  await createPluginContainer({}, [plugin], { environment: "ssr" }).generateBundle((asset) => emitted.push(asset));
  assert.equal(emitted.length, 1);
  assert.equal(emitted[0].fileName, "server-manifest.json");
});

// PR #54
test("findConfig discovers CommonJS Vite configuration formats", () => {
  for (const name of ["vite.config.cjs", "vite.config.cts"]) {
    const root = mkdtempSync(join(tmpdir(), "oj-config-format-"));
    try {
      const config = join(root, name);
      writeFileSync(config, "module.exports = {};\n");
      assert.equal(findConfig(root), config);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  }
});

// PR #55
test("plugin container preserves an explicitly disabled Vite public directory", async () => {
  const root = mkdtempSync(join(tmpdir(), "oj-no-public-"));
  try {
    const vite = join(root, "node_modules", "vite");
    mkdirSync(vite, { recursive: true });
    writeFileSync(join(root, "package.json"), '{"name":"synthetic-app"}');
    writeFileSync(join(root, "vite.config.mjs"), "export default {};\n");
    writeFileSync(join(vite, "package.json"), '{"name":"vite","type":"module","main":"./index.mjs"}');
    writeFileSync(
      join(vite, "index.mjs"),
      "export async function loadConfigFromFile() { return { config: { plugins: [], publicDir: false } }; }\n",
    );

    assert.equal((await loadPluginContainer(root)).publicDir, false);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

// PR #43
test("plugin hooks receive the configured Vite mode", async () => {
  const plugin = {
    name: "synthetic-mode-transform",
    transform() {
      return `export default ${JSON.stringify(this.environment.config.mode)};`;
    },
  };

  const staging = createPluginContainer({}, [plugin], { command: "serve", mode: "staging" });
  const preview = createPluginContainer({}, [plugin], { command: "build", mode: "preview" });
  const defaults = createPluginContainer({}, [plugin], { command: "build" });

  assert.equal(await staging.transform("", "/app.ts"), 'export default "staging";');
  assert.equal(await preview.transform("", "/app.ts"), 'export default "preview";');
  assert.equal(await defaults.transform("", "/app.ts"), 'export default "production";');
});

// PR #44
test("resolveId and load receive Vite SSR hook options", async () => {
  const plugin = {
    name: "synthetic-environment-module",
    resolveId(source, _importer, options) {
      return `\0${options?.ssr ? "server" : "client"}:${source}`;
    },
    load(_id, options) {
      return `export default ${JSON.stringify(options?.ssr ? "server" : "client")};`;
    },
  };

  const server = createPluginContainer({}, [plugin], { environment: "ssr" });
  const client = createPluginContainer({}, [plugin], { environment: "client" });

  assert.equal(await server.resolveId("virtual:entry", "/app.ts"), "\0server:virtual:entry");
  assert.equal(await server.load("\0server:virtual:entry"), 'export default "server";');
  assert.equal(await client.resolveId("virtual:entry", "/app.ts"), "\0client:virtual:entry");
  assert.equal(await client.load("\0client:virtual:entry"), 'export default "client";');
});

// PR #52
test("transform hooks receive the active SSR environment option", async () => {
  const plugin = {
    name: "synthetic-environment-transform",
    transform(_code, _id, options) {
      return `export default ${JSON.stringify(options?.ssr ? "server" : "client")};`;
    },
  };

  const server = createPluginContainer({}, [plugin], { environment: "ssr" });
  const client = createPluginContainer({}, [plugin], { environment: "client" });

  assert.equal(await server.transform("", "/page.mdx"), 'export default "server";');
  assert.equal(await client.transform("", "/page.mdx"), 'export default "client";');
});

// PR #41
test("buildStart runs plugins whose only hook initializes generated sources", async () => {
  let initialized = 0;
  const container = createPluginContainer({}, [
    {
      name: "synthetic-source-generator",
      buildStart() {
        initialized++;
      },
    },
  ]);

  await container.buildStart();
  await container.buildStart();

  assert.equal(initialized, 1);
});

// PR #67
test("moduleParsed plugins observe transformed framework and user modules", async () => {
  const parsed = [];
  const container = createPluginContainer({}, [
    {
      name: "synthetic-module-graph-observer",
      moduleParsed(info) {
        parsed.push({ id: info.id, code: info.code, importedIds: info.importedIds });
      },
    },
    {
      name: "synthetic-module-transform",
      transform(code) {
        return `${code}\ntransformed();`;
      },
    },
  ]);

  await container.transform("entry();", "/entry.ts");
  await container.transformUserCode("page();", "/page.ts");

  assert.deepEqual(parsed, [
    { id: "/entry.ts", code: "entry();\ntransformed();", importedIds: [] },
    { id: "/page.ts", code: "page();\ntransformed();", importedIds: [] },
  ]);
});

// PR #66
test("plugin hooks can inspect loaded and transformed module metadata", async () => {
  const dependencyId = "\0synthetic:dependency";
  const dependencyCode = "export const value = 42;";
  const container = createPluginContainer({}, [
    {
      name: "synthetic-virtual-module",
      load(id) {
        return id === dependencyId ? dependencyCode : null;
      },
    },
    {
      name: "synthetic-module-graph-inspector",
      transform(code, id) {
        const dependency = this.getModuleInfo(dependencyId);
        const current = this.getModuleInfo(id);
        const moduleIds = [...this.getModuleIds()];
        return [
          dependency.code,
          dependency.importedIds.length,
          current.code === code,
          moduleIds.includes(dependencyId),
          moduleIds.includes(id),
        ].join(":");
      },
    },
  ]);

  assert.equal(await container.load(dependencyId), dependencyCode);
  assert.equal(await container.transform("export default 1;", "/app.ts"), "export const value = 42;:0:true:true:true");
});

// PR #65
test("plugin hook contexts load virtual dependency modules", async () => {
  const container = createPluginContainer({}, [
    {
      name: "synthetic-virtual-module-loader",
      load(id) {
        return id === "\0synthetic:dependency" ? "export const value = 42;" : null;
      },
    },
    {
      name: "synthetic-dependency-transform",
      async transform(_code, id) {
        if (id !== "/app.ts") return null;
        const dependency = await this.load({ id: "\0synthetic:dependency" });
        return `export default ${JSON.stringify(dependency.code)};`;
      },
    },
  ]);

  assert.equal(await container.transform("", "/app.ts"), 'export default "export const value = 42;";');
});

// PR #63
test("plugin hook contexts resolve virtual dependencies without reentering themselves", async () => {
  let resolverCalls = 0;
  const container = createPluginContainer({}, [
    {
      name: "synthetic-delegating-resolver",
      async resolveId(source, importer) {
        if (source !== "virtual:entry") return null;
        resolverCalls++;
        const resolved = await this.resolve(source, importer, { skipSelf: true });
        return `${resolved.id}?wrapped`;
      },
    },
    {
      name: "synthetic-fallback-resolver",
      resolveId(source) {
        return source.startsWith("virtual:") ? `\0resolved:${source.slice(8)}` : null;
      },
    },
    {
      name: "synthetic-dependency-loader",
      async load(id) {
        if (id !== "\0resolved:entry?wrapped") return null;
        const dependency = await this.resolve("virtual:dependency", id);
        return `export default ${JSON.stringify(dependency.id)};`;
      },
    },
  ]);

  assert.equal(await container.resolveId("virtual:entry", "/app.ts"), "\0resolved:entry?wrapped");
  assert.equal(resolverCalls, 1);
  assert.equal(await container.load("\0resolved:entry?wrapped"), 'export default "\\u0000resolved:dependency";');
});

// PR #49
test("transform hook code filters gate both transform entry points", async () => {
  const plugin = {
    name: "synthetic-selective-transform",
    transform: {
      filter: { id: /\.tsx$/, code: { include: /@enabled/, exclude: /@disabled/ } },
      handler(code) {
        return `${code}\ntransformed();`;
      },
    },
  };
  const container = createPluginContainer({}, [plugin]);

  assert.equal(await container.transform("plain();", "/app.tsx"), null);
  assert.equal(await container.transform("/* @enabled @disabled */", "/app.tsx"), null);
  assert.equal(await container.transform("/* @enabled */", "/app.tsx"), "/* @enabled */\ntransformed();");
  assert.equal(await container.transformUserCode("plain();", "/app.tsx"), null);
  assert.equal(await container.transformUserCode("/* @enabled */", "/app.tsx"), "/* @enabled */\ntransformed();");
});

// PR #50
test("transform hooks honor per-hook pre and post ordering", async () => {
  const plugin = (name, order) => ({
    name,
    transform: {
      ...(order ? { order } : {}),
      handler(code) {
        return `${code}${name};`;
      },
    },
  });
  const container = createPluginContainer({}, [plugin("post", "post"), plugin("normal"), plugin("pre", "pre")]);

  assert.equal(await container.transform("", "/app.ts"), "pre;normal;post;");
  assert.equal(await container.transformUserCode("", "/app.ts"), "pre;normal;post;");
});

// PR #68
test("configResolved initializes plugin state once before concurrent module hooks", async () => {
  let resolved;
  let initializations = 0;
  const plugin = {
    name: "synthetic-config-initialized-loader",
    async configResolved(config) {
      initializations++;
      await Promise.resolve();
      resolved = config;
    },
    resolveId(source) {
      return `${resolved.root}/${source}`;
    },
    load() {
      return `export default ${JSON.stringify(`${resolved.command}:${resolved.mode}:${resolved.base}`)};`;
    },
    transform(code) {
      return `${code}:${resolved.plugins.length}`;
    },
  };
  const container = createPluginContainer({}, [plugin], {
    command: "serve",
    mode: "staging",
    environment: "ssr",
    config: { root: "/synthetic-app", base: "/preview/" },
  });

  const [loaded, transformed, id] = await Promise.all([
    container.load("virtual:entry"),
    container.transform("module", "/entry.ts"),
    container.resolveId("virtual:entry", "/entry.ts"),
  ]);

  assert.equal(loaded, 'export default "serve:staging:/preview/";');
  assert.equal(transformed, "module:1");
  assert.equal(id, "/synthetic-app/virtual:entry");
  assert.equal(initializations, 1);
});

// PR #69
test("plugin config hooks initialize state and merge nested configuration", async () => {
  const root = mkdtempSync(join(tmpdir(), "oj-plugin-config-hooks-"));
  try {
    const vite = join(root, "node_modules", "vite");
    mkdirSync(vite, { recursive: true });
    writeFileSync(join(root, "package.json"), '{"name":"synthetic-app"}');
    writeFileSync(join(root, "vite.config.mjs"), "export default {};\n");
    writeFileSync(join(vite, "package.json"), '{"name":"vite","type":"module","main":"./index.mjs"}');
    writeFileSync(
      join(vite, "index.mjs"),
      `
      export async function loadConfigFromFile() {
        let observed;
        return {
          config: {
            define: { EXISTING: "preserved" },
            resolve: { alias: ["initial"] },
            plugins: [
              {
                name: "synthetic-unsupported-config",
                config(config) {
                  return config.build.rollupOptions;
                },
              },
              {
                name: "synthetic-config-producer",
                async config(_config, environment) {
                  return {
                    define: { TOKEN: environment.command + ":" + environment.mode },
                    resolve: { alias: ["configured"] },
                    publicDir: "configured-public",
                  };
                },
              },
              {
                name: "synthetic-config-consumer",
                config(config) {
                  observed = [config.define.TOKEN, config.define.EXISTING, config.resolve.alias.join(",")].join("|");
                },
                transform() {
                  return observed ? "export default " + JSON.stringify(observed) + ";" : null;
                },
              },
            ],
          },
        };
      }
    `,
    );

    const container = await loadPluginContainer(root, { command: "serve", mode: "staging" });

    assert.equal(
      await container.transform("", "/app.ts"),
      'export default "serve:staging|preserved|initial,configured";',
    );
    assert.equal(container.publicDir, "configured-public");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("codeAllowed: string code filters are substrings, RegExps reset, never path globs", () => {
  assert.ok(codeAllowed({ code: "import.meta.glob" }, 'const m = import.meta.glob("./x");'));
  assert.ok(!codeAllowed({ code: "import.meta.glob" }, "plain();"));
  assert.ok(codeAllowed({ code: { include: [/@enabled/], exclude: "@disabled" } }, "/* @enabled */"));
  assert.ok(!codeAllowed({ code: { include: [/@enabled/], exclude: "@disabled" } }, "/* @enabled @disabled */"));
  const sticky = /marker/y;
  assert.ok(codeAllowed({ code: sticky }, "marker one") && codeAllowed({ code: sticky }, "marker two"));
  assert.ok(codeAllowed({ id: /x/ }, "anything"), "no code filter admits any source");
});

test("per-hook order applies to resolveId and load, not only transform", async () => {
  const seen = [];
  const plugin = (name, order) => ({
    name,
    resolveId: {
      ...(order ? { order } : {}),
      handler(id) {
        seen.push(`resolve:${name}`);
        return null;
      },
    },
    load: {
      ...(order ? { order } : {}),
      handler() {
        seen.push(`load:${name}`);
        return null;
      },
    },
  });
  const container = createPluginContainer({}, [plugin("post", "post"), plugin("normal"), plugin("pre", "pre")]);
  await container.resolveId("virtual:x", "/app.ts");
  await container.load("virtual:x");
  assert.deepEqual(seen, ["resolve:pre", "resolve:normal", "resolve:post", "load:pre", "load:normal", "load:post"]);
});

test("module info carries meta and moduleParsed sees the same record", async () => {
  let parsed;
  const container = createPluginContainer({}, [
    {
      name: "observer",
      moduleParsed(info) {
        parsed = info;
      },
      transform() {
        return "changed();";
      },
    },
  ]);
  await container.transform("orig();", "/app.ts");
  assert.deepEqual(parsed, { id: "/app.ts", code: "changed();", importedIds: [], meta: {} });
});

test("config and configResolved hooks skip the plugins oj reimplements", async () => {
  const ran = [];
  const container = createPluginContainer({}, [
    {
      name: "tanstack-start-core:config",
      configResolved() {
        ran.push("tanstack");
      },
      transform() {
        return null;
      },
    },
    {
      name: "vite:react-babel",
      configResolved() {
        ran.push("vite");
      },
      transform() {
        return null;
      },
    },
    {
      name: "app-plugin",
      configResolved() {
        ran.push("app");
      },
      transform() {
        return null;
      },
    },
  ]);
  await container.transform("x", "/app.ts");
  assert.deepEqual(ran, ["app"]);
});

test("config hooks see the command's default mode when none is given", async () => {
  const root = mkdtempSync(join(tmpdir(), "oj-config-mode-"));
  try {
    const vite = join(root, "node_modules", "vite");
    mkdirSync(vite, { recursive: true });
    writeFileSync(join(root, "package.json"), '{"name":"synthetic-app"}');
    writeFileSync(join(root, "vite.config.mjs"), "export default {};\n");
    writeFileSync(join(vite, "package.json"), '{"name":"vite","type":"module","main":"./index.mjs"}');
    writeFileSync(
      join(vite, "index.mjs"),
      `
      export async function loadConfigFromFile() {
        let seen;
        return { config: { plugins: [{
          name: "mode-probe",
          config(_c, env) { seen = env.command + ":" + env.mode; },
          transform() { return "export default " + JSON.stringify(seen) + ";"; },
        }] } };
      }
    `,
    );
    const build = await loadPluginContainer(root, { command: "build" });
    assert.equal(await build.transform("", "/a.ts"), 'export default "build:production";');
    const serve = await loadPluginContainer(root, { command: "serve" });
    assert.equal(await serve.transform("", "/a.ts"), 'export default "serve:development";');
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

// PR #73
test("retains configResolved-only plugins alongside operational consumers", () => {
  const initializer = {
    name: "synthetic-lifecycle-initializer",
    configResolved() {},
  };
  const consumer = {
    name: "synthetic-lifecycle-consumer",
    load(id) {
      return id === "virtual:synthetic" ? "export default true" : null;
    },
  };

  const container = createPluginContainer({}, [initializer, consumer]);

  assert.equal(container.pluginCount, 2);
});

// PR #76
test("renderChunk retains chunk-only plugins and chains applicable hooks", async () => {
  const plugins = [
    {
      name: "synthetic-server-only",
      applyToEnvironment: (environment) => environment.config.consumer === "server",
      renderChunk: () => "incorrect server chunk",
    },
    {
      name: "synthetic-post-chunk",
      enforce: "post",
      renderChunk: {
        handler(code, chunk) {
          return chunk.isEntry ? { code: `${code}/*post*/` } : null;
        },
      },
    },
    {
      name: "synthetic-pre-chunk",
      enforce: "pre",
      renderChunk(code, chunk) {
        return chunk.isEntry ? `/*${this.environment.config.consumer}*/${code}` : null;
      },
    },
  ];
  const container = createPluginContainer({}, plugins, { command: "build", environment: "client" });
  const entry = { type: "chunk", fileName: "assets/entry.js", isEntry: true, code: "entry();" };

  assert.equal(container.pluginCount, 3);
  assert.equal(await container.renderChunk(entry.code, entry), "/*client*/entry();/*post*/");
  assert.equal(await container.renderChunk("chunk();", { ...entry, isEntry: false }), null);
});

// PR #78
test("writeBundle observes emitted chunks after generation in the client environment", async () => {
  const lifecycle = [];
  const bundle = {
    "assets/entry.js": { type: "chunk", fileName: "assets/entry.js", isEntry: true, code: "entry();" },
    "assets/chunk.js": { type: "chunk", fileName: "assets/chunk.js", isEntry: false, code: "chunk();" },
  };
  const plugins = [
    {
      name: "synthetic-post-writer",
      enforce: "post",
      writeBundle: {
        handler(options, output, isWrite) {
          lifecycle.push(`post:${options.format}:${Object.keys(output).sort().join(",")}:${isWrite}`);
        },
      },
    },
    {
      name: "synthetic-server-writer",
      applyToEnvironment: (environment) => environment.config.consumer === "server",
      writeBundle() {
        lifecycle.push("server");
      },
    },
    {
      name: "synthetic-pre-writer",
      enforce: "pre",
      writeBundle(_options, output) {
        lifecycle.push(`pre:${this.environment.config.consumer}:${output["assets/entry.js"].code}`);
      },
    },
    {
      name: "synthetic-generator",
      generateBundle() {
        lifecycle.push("generate");
      },
    },
  ];
  const container = createPluginContainer({}, plugins, { command: "build", environment: "client" });

  assert.equal(container.pluginCount, 4);
  await container.generateBundle(() => {});
  await container.writeBundle(bundle);

  assert.deepEqual(lifecycle, ["generate", "pre:client:entry();", "post:es:assets/chunk.js,assets/entry.js:true"]);
});

// PR #83
test("closeBundle runs finalizer-only plugins in their matching environment", async () => {
  const finalized = [];
  const plugin = {
    name: "synthetic-build-finalizer",
    applyToEnvironment: (environment) => environment.name === "client",
    closeBundle() {
      finalized.push(this.environment.name);
    },
  };

  const client = createPluginContainer({}, [plugin], { command: "build", environment: "client" });
  const server = createPluginContainer({}, [plugin], { command: "build", environment: "ssr" });

  assert.equal(client.pluginCount, 1);
  await client.closeBundle();
  await server.closeBundle();

  assert.deepEqual(finalized, ["client"]);
});

test("closeBundle supports object-form finalizers and continues past failed hooks", async () => {
  const finalized = [];
  const container = createPluginContainer(
    {},
    [
      {
        name: "synthetic-failed-finalizer",
        closeBundle() {
          throw new Error("synthetic finalizer failure");
        },
      },
      {
        name: "synthetic-object-finalizer",
        closeBundle: {
          handler() {
            finalized.push(this.environment.config.consumer);
          },
        },
      },
    ],
    { command: "build", environment: "ssr" },
  );

  await container.closeBundle();

  assert.deepEqual(finalized, ["server"]);
});

test("production Start builds close both environment plugin containers", () => {
  const source = readFileSync(new URL("../../crates/oj_server/src/assets/start/build.mjs", import.meta.url), "utf8");

  assert.match(source, /await clientContainer\?\.closeBundle\(\)/);
  assert.match(source, /await serverContainer\?\.closeBundle\(\)/);
});

// PR #85
test("buildEnd runs completion-only plugins in their matching environment", async () => {
  const completed = [];
  const failure = new Error("synthetic build failure");
  const plugin = {
    name: "synthetic-build-completion",
    applyToEnvironment: (environment) => environment.name === "ssr",
    buildEnd(error) {
      completed.push({ environment: this.environment.name, error });
    },
  };

  const client = createPluginContainer({}, [plugin], { command: "build", environment: "client" });
  const server = createPluginContainer({}, [plugin], { command: "build", environment: "ssr" });

  assert.equal(server.pluginCount, 1);
  await client.buildEnd();
  await server.buildEnd(failure);

  assert.deepEqual(completed, [{ environment: "ssr", error: failure }]);
});

test("buildEnd supports object-form hooks and continues past failed callbacks", async () => {
  const completed = [];
  const container = createPluginContainer(
    {},
    [
      {
        name: "synthetic-failed-completion",
        buildEnd() {
          throw new Error("synthetic completion failure");
        },
      },
      {
        name: "synthetic-object-completion",
        buildEnd: {
          handler(error) {
            completed.push({ consumer: this.environment.config.consumer, error });
          },
        },
      },
    ],
    { command: "build", environment: "client" },
  );

  await container.buildEnd();

  assert.deepEqual(completed, [{ consumer: "client", error: undefined }]);
});

// PR #87
test("renderStart runs output-only plugins with their environment and bundle options", async () => {
  const rendered = [];
  const output = { format: "es", dir: "/synthetic/dist" };
  const input = { input: "/synthetic/entry.ts" };
  const plugin = {
    name: "synthetic-render-initializer",
    applyToEnvironment: (environment) => environment.name === "ssr",
    renderStart(outputOptions, inputOptions) {
      rendered.push({ environment: this.environment.name, outputOptions, inputOptions });
    },
  };

  const client = createPluginContainer({}, [plugin], { command: "build", environment: "client" });
  const server = createPluginContainer({}, [plugin], { command: "build", environment: "ssr" });

  assert.equal(server.pluginCount, 1);
  await client.renderStart(output, input);
  await server.renderStart(output, input);

  assert.deepEqual(rendered, [{ environment: "ssr", outputOptions: output, inputOptions: input }]);
});

test("renderStart supports object-form hooks and continues past failed initializers", async () => {
  const rendered = [];
  const container = createPluginContainer(
    {},
    [
      {
        name: "synthetic-failed-render",
        renderStart() {
          throw new Error("synthetic render failure");
        },
      },
      {
        name: "synthetic-object-render",
        renderStart: {
          handler(output) {
            rendered.push({ consumer: this.environment.config.consumer, output });
          },
        },
      },
    ],
    { command: "build", environment: "client" },
  );
  const output = { format: "es" };

  await container.renderStart(output, {});

  assert.deepEqual(rendered, [{ consumer: "client", output }]);
});

// PR #71
test("generateBundle receives and can mutate actual emitted chunks", async () => {
  const emitted = [];
  const bundle = {
    "assets/entry.js": { type: "chunk", fileName: "assets/entry.js", isEntry: true, code: "entry();" },
    "assets/chunk.js": { type: "chunk", fileName: "assets/chunk.js", isEntry: false, code: "chunk();" },
  };
  const plugin = {
    name: "synthetic-bundle-reporter",
    generateBundle(_options, output) {
      const chunks = Object.values(output).filter((entry) => entry.type === "chunk");
      this.emitFile({ type: "asset", fileName: "report.txt", source: `chunks:${chunks.length}` });
      for (const chunk of chunks) {
        if (chunk.isEntry) chunk.code = `/* generated */${chunk.code}`;
      }
    },
  };

  await createPluginContainer({}, [plugin], { command: "build" }).generateBundle(
    (asset) => emitted.push(asset),
    bundle,
  );

  assert.deepEqual(emitted, [{ type: "asset", fileName: "report.txt", source: "chunks:2" }]);
  assert.equal(bundle["assets/entry.js"].code, "/* generated */entry();");
});

// PR #79
test("plugin hooks inspect environment modules by ID, URL, and shared source file", async () => {
  const file = "/synthetic/dependency.ts";
  const first = `${file}?first`;
  const second = `${file}?second`;
  const container = createPluginContainer({}, [
    {
      name: "synthetic-environment-module-source",
      load(id) {
        return id.startsWith(file) ? `export default ${JSON.stringify(id)};` : null;
      },
    },
    {
      name: "synthetic-environment-module-inspector",
      async transform(code, id) {
        const graph = this.environment.moduleGraph;
        const byId = graph.getModuleById(first);
        const byUrl = await graph.getModuleByUrl(first);
        const byFile = graph.getModulesByFile(file);
        const current = graph.getModuleById(id);
        return JSON.stringify({
          sameModule: byId === byUrl,
          environment: byId.environment,
          file: byId.file,
          loadedCode: byId.transformResult.code,
          fileVariants: byFile.size,
          includesBoth: byFile.has(byId) && byFile.has(graph.getModuleById(second)),
          currentCode: current.transformResult.code,
          missingId: graph.getModuleById("/missing.ts") === undefined,
          missingFile: graph.getModulesByFile("/missing.ts") === undefined,
        });
      },
    },
  ]);

  await container.load(first);
  await container.load(second);

  assert.equal(
    await container.transform("export default 1;", "/synthetic/entry.ts"),
    JSON.stringify({
      sameModule: true,
      environment: "client",
      file,
      loadedCode: `export default ${JSON.stringify(first)};`,
      fileVariants: 2,
      includesBoth: true,
      currentCode: "export default 1;",
      missingId: true,
      missingFile: true,
    }),
  );
});

test("client and SSR plugin environments maintain independent module graphs", async () => {
  const observed = [];
  const plugin = {
    name: "synthetic-environment-isolation",
    transform(code, id) {
      const graph = this.environment.moduleGraph;
      observed.push({ environment: this.environment.name, graph, code: graph.getModuleById(id).transformResult.code });
      return `${this.environment.name}:${code}`;
    },
  };
  const client = createPluginContainer({}, [plugin], { environment: "client" });
  const ssr = createPluginContainer({}, [plugin], { environment: "ssr" });

  assert.equal(await client.transform("browser", "/shared.ts"), "client:browser");
  assert.equal(await ssr.transformUserCode("server", "/shared.ts"), "ssr:server");
  assert.deepEqual(
    observed.map(({ environment, code }) => ({ environment, code })),
    [
      { environment: "client", code: "browser" },
      { environment: "ssr", code: "server" },
    ],
  );
  assert.notEqual(observed[0].graph, observed[1].graph);
});

// PR #72
test("plugin environment exposes a resolver for aliases, configured extensions, and packages", async () => {
  const root = realpathSync(mkdtempSync(join(tmpdir(), "oj-plugin-create-resolver-")));
  try {
    const source = join(root, "src");
    const components = join(source, "components");
    const packageDirectory = join(root, "node_modules", "synthetic-package");
    mkdirSync(components, { recursive: true });
    mkdirSync(packageDirectory, { recursive: true });
    writeFileSync(join(root, "package.json"), '{"name":"synthetic-app"}');
    writeFileSync(join(components, "card.tsx"), "export default 1;");
    writeFileSync(join(source, "theme.widget"), "export default 2;");
    writeFileSync(join(packageDirectory, "package.json"), '{"name":"synthetic-package","main":"index.js"}');
    writeFileSync(join(packageDirectory, "index.js"), "module.exports = 3;");

    const container = createPluginContainer(
      {},
      [
        {
          name: "synthetic-config-resolver",
          async transform() {
            const resolve = this.environment.config.createResolver();
            const importer = join(source, "entry.ts");
            return JSON.stringify(
              await Promise.all([
                resolve("@/components/card", importer),
                resolve("./theme", importer),
                resolve("synthetic-package", importer),
                resolve("./missing", importer),
              ]),
            );
          },
        },
      ],
      {
        config: {
          root,
          resolve: { alias: { "@": source }, extensions: [".widget", ".tsx", ".js"] },
        },
      },
    );

    assert.equal(
      await container.transform("", join(source, "entry.ts")),
      JSON.stringify([
        join(components, "card.tsx"),
        join(source, "theme.widget"),
        join(packageDirectory, "index.js"),
        null,
      ]),
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("plugin environment preserves an existing configured resolver", async () => {
  const existing = () => async (id) => `/configured/${id}`;
  const container = createPluginContainer(
    {},
    [
      {
        name: "synthetic-existing-resolver",
        async transform() {
          return this.environment.config.createResolver()("entry");
        },
      },
    ],
    { config: { createResolver: existing } },
  );

  assert.equal(await container.transform("", "/entry.ts"), "/configured/entry");
});
