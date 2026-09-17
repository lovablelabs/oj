// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import { asset } from "./harness.mjs";
import { detectSsrRunnerBacked, extractAlias, extractOptimizeDeps, extractProxy, extractResolve, extractSsr, mergeConfigLite, warnUnsupported } from "../../crates/oj_server/src/assets/vite-extract.mjs";

test("optimizeDeps carries needsInterop and force alongside the lists", () => {
  const out = extractOptimizeDeps({
    include: ["a", "pkg/*"],
    exclude: "b",
    needsInterop: ["cjs-ish", 42],
    force: true,
    esbuildOptions: { target: "es2020" },
  });
  assert.deepEqual(out, { include: ["a", "pkg/*"], exclude: ["b"], needsInterop: ["cjs-ish"], force: true });
  assert.equal(extractOptimizeDeps({ force: "yes" }), null, "non-boolean force is ignored");
  assert.equal(extractOptimizeDeps(undefined), null);
});

test("string aliases pass through unchanged", () => {
  const out = extractAlias({ "@app": "/src", "~": "/src/lib" });
  assert.deepEqual(out, { "@app": "/src", "~": "/src/lib" });
});

test("array-form string aliases are read from find/replacement", () => {
  const out = extractAlias([{ find: "@app", replacement: "/src" }]);
  assert.deepEqual(out, { "@app": "/src" });
});

test("duplicate array-form aliases preserve Vite first-match precedence", () => {
  const out = extractAlias([
    { find: "@app", replacement: "/first/src" },
    { find: "@app", replacement: "/second/src" },
  ]);

  assert.equal(out["@app"], "/first/src");
});

test("a monorepo regex alias pair collapses to one directory alias", () => {
  // The standard TanStack/monorepo shape: an exact match to the package entry
  // plus a subpath match. Both must collapse to `@x/pkg -> .../src`.
  const out = extractAlias([
    { find: /^@excalidraw\/excalidraw$/, replacement: "/repo/packages/excalidraw/index.ts" },
    { find: /^@excalidraw\/excalidraw\/(.*)/, replacement: "/repo/packages/excalidraw/$1" },
  ]);
  assert.equal(out["@excalidraw/excalidraw"], "/repo/packages/excalidraw");
});

test("subpath-only regex derives the key and strips the trailing capture", () => {
  const out = extractAlias([{ find: /^@x\/pkg\/(.*)$/, replacement: "/abs/pkg/$1" }]);
  assert.equal(out["@x/pkg"], "/abs/pkg");
});

test("index.ext replacement suffix is stripped to the directory", () => {
  const out = extractAlias([{ find: /^lib$/, replacement: "/abs/lib/index.tsx" }]);
  assert.equal(out["lib"], "/abs/lib");
});

test("a regex too complex to express as a path alias is skipped, not mis-resolved", () => {
  const out = extractAlias([{ find: /^@x\/.*\/deep/, replacement: "/abs/$1" }]);
  assert.ok(!("@x" in out), `must not produce a bogus key: ${JSON.stringify(out)}`);
});

test("non-string replacements are ignored", () => {
  const out = extractAlias([{ find: "@fn", replacement: () => "/x" }]);
  assert.deepEqual(out, {});
});

function captureStderr(fn) {
  const lines = [];
  const write = process.stderr.write;
  process.stderr.write = (chunk) => { lines.push(String(chunk)); return true; };
  try { fn(); } finally { process.stderr.write = write; }
  return lines.join("");
}

test("proxy contexts pass through verbatim, regex (^) contexts included", () => {
  const out = extractProxy({
    "/api": "http://localhost:3000",
    "^/re/.*": { target: "http://localhost:3001", ws: true, changeOrigin: true },
  });
  assert.deepEqual(out, {
    "/api": "http://localhost:3000",
    "^/re/.*": { target: "http://localhost:3001", ws: true, changeOrigin: true },
  });
});

test("function-valued proxy options: the extracted entry drops them, but rewrite no longer warns (the plugin host applies it)", () => {
  let out;
  const err = captureStderr(() => {
    out = extractProxy({
      "/api": {
        target: "http://localhost:3000",
        rewrite: (p) => p.replace(/^\/api/, ""),
        configure: () => {},
        bypass: () => null,
      },
    });
  });
  assert.deepEqual(out, { "/api": { target: "http://localhost:3000" } });
  // A function rewrite is honored by the plugin host's single proxy, so the
  // extractor no longer warns that it is dropped.
  assert.doesNotMatch(err, /rewrite is a function/);
  // configure needs http-proxy (bundled into Vite); the warning is honest that
  // it applies only when http-proxy is resolvable. bypass runs on the host proxy.
  assert.match(err, /server\.proxy\["\/api"\]\.configure is a function; applied only when http-proxy is resolvable/);
  assert.match(err, /server\.proxy\["\/api"\]\.bypass is a function; applied by the plugin host proxy/);
});


test("Vite's own client aliases (@vite/env, @vite/client) are skipped without a warning", () => {
  let out;
  const stderr = captureStderr(() => {
    out = extractAlias([
      { find: /^\/?@vite\/env/, replacement: "/vite/dist/client/env.mjs" },
      { find: /^\/?@vite\/client/, replacement: "/vite/dist/client/client.mjs" },
      { find: "@", replacement: "/app/src" },
    ]);
  });
  assert.deepEqual(out, { "@": "/app/src" });
  assert.equal(stderr, "", "Vite's built-in aliases are not the user's configuration");
});

test("warnUnsupported reports only options the user's config sets", () => {
  // The shape resolveConfig produces for a config that sets none of these: all
  // defaults Vite injects. Called with the RAW config instead, nothing is reported.
  const resolvedDefaults = {
    esbuild: { jsxDev: true, charset: "utf8", legalComments: "none" },
    optimizeDeps: { esbuildOptions: { preserveSymlinks: false } },
    worker: { format: "iife", plugins: () => [] },
    ssr: { resolve: { conditions: [], externalConditions: [] } },
    server: { cors: { origin: /^https?:\/\/(?:(?:[^:]+\.)?localhost|127\.0\.0\.1|\[::1\])(?::\d+)?$/ } },
    build: { terserOptions: {} },
  };
  assert.notEqual(captureStderr(() => warnUnsupported(resolvedDefaults)), "", "the resolved shape would warn");
  const rawUserConfig = { plugins: [], tanstackStart: { server: { entry: "server" } } };
  assert.equal(captureStderr(() => warnUnsupported(rawUserConfig)), "");
  assert.equal(captureStderr(() => warnUnsupported(null)), "");
  const userSets = captureStderr(() => warnUnsupported({ worker: { format: "es" }, esbuild: { charset: "ascii" }, build: { terserOptions: { compress: true } } }));
  assert.match(userSets, /worker config is not applied/);
  assert.match(userSets, /esbuild options charset are not applied/);
  assert.match(userSets, /build.terserOptions is not applied/);

  // ssr.resolve.conditions/externalConditions ARE applied (the preferred
  // source for the Node SSR consumers): no stale blanket warning for them.
  const applied = captureStderr(() =>
    warnUnsupported({ ssr: { resolve: { conditions: ["workerd"], externalConditions: ["workerd"] } } }),
  );
  assert.equal(applied, "", "applied ssr.resolve subkeys must not warn");
  // The genuinely-inert subkeys still do, named individually.
  const inert = captureStderr(() =>
    warnUnsupported({ ssr: { resolve: { conditions: ["workerd"], mainFields: ["module"], extensions: [".ts"] } } }),
  );
  assert.match(inert, /ssr\.resolve\.mainFields\/extensions is not applied \(conditions\/externalConditions are\)/);
});

test("resolve.externalConditions is extracted, top-level and through the ssr sugar", () => {
  assert.deepEqual(
    extractResolve({ conditions: ["custom"], externalConditions: ["custom-ext"] }),
    { conditions: ["custom"], externalConditions: ["custom-ext"] },
  );

  // Vite's `ssr.resolve` sugar carries it too.
  const viaSsr = extractSsr({ noExternal: ["dep"], resolve: { externalConditions: ["workerd-ext"] } });
  assert.deepEqual(viaSsr.resolve, { externalConditions: ["workerd-ext"] });
  assert.deepEqual(viaSsr.noExternal, ["dep"]);

  // The environments.ssr spelling wins over ssr.resolve where both are set.
  const viaEnv = extractSsr(
    { resolve: { externalConditions: ["old"] } },
    { resolve: { externalConditions: ["env-ext"], conditions: ["workerd"] } },
  );
  assert.deepEqual(viaEnv.resolve, { conditions: ["workerd"], externalConditions: ["env-ext"] });

  // An environments-only config still produces the ssr resolve block.
  const envOnly = extractSsr(undefined, { resolve: { externalConditions: ["only-env"] } });
  assert.deepEqual(envOnly.resolve, { externalConditions: ["only-env"] });
});

// The runner-backed signal is Vite's declaration mechanism, never a vendor
// name check: the RAW config, or a plugin's `config` hook return merged
// Vite-style (runConfigHook), declares `environments.<name>.dev.createEnvironment`.
// The RESOLVED config's environments are deliberately not consulted: Vite
// fills every environment's dev.createEnvironment with its own default
// factory, so presence there says nothing.
test("detectSsrRunnerBacked: raw or config-hook environment declaration", async () => {
  // A user-declared custom dev runtime factory in the RAW config.
  const rawWithFactory = { environments: { ssr: { dev: { createEnvironment: () => ({}) } } } };
  assert.equal(await detectSsrRunnerBacked(rawWithFactory), true);

  // A plugin declaring its environment from its `config` hook, in a raw
  // (nested, factory-returned) plugin list. The name does not matter — any
  // declaring plugin counts (the Cloudflare plugin is one such).
  const declaring = {
    name: "acme-runner",
    config: () => ({ environments: { worker: { dev: { createEnvironment: () => ({}) } } } }),
  };
  assert.equal(await detectSsrRunnerBacked({ plugins: [[{ name: "react" }, [declaring]]] }), true);

  // The Cloudflare plugin's own shape (vite-plugin-cloudflare:config returns
  // environments from its config hook) must be seen — it is on no skip list.
  const cfConfig = {
    name: "vite-plugin-cloudflare:config",
    config: () => ({ environments: { ssr: { dev: { createEnvironment: () => ({}) } } } }),
  };
  assert.equal(await detectSsrRunnerBacked({ plugins: [{ name: "vite-plugin-cloudflare" }, cfConfig] }), true);

  // Object-form hook with `order: "pre"` runs first (getSortedPluginsByHook),
  // and later hooks see earlier merges (runConfigHook merges successively):
  // `late` declares only if `early` already contributed its marker.
  const early = { name: "early", config: { order: "pre", handler: () => ({ custom: { flag: true } }) } };
  const late = {
    name: "late",
    config: {
      handler: (conf) =>
        conf.custom?.flag
          ? { environments: { ssr: { dev: { createEnvironment: () => ({}) } } } }
          : null,
    },
  };
  assert.equal(await detectSsrRunnerBacked({ plugins: [late, early] }), true);

  // A skipped plugin's hook is never run, so its declaration is invisible:
  // the skip exists for side effects (the router plugin's config hook starts
  // its generator), and not running the hook is the only way to avoid them —
  // a return cannot be captured without executing it.
  let tanstackRan = false;
  const skipped = {
    name: "tanstack-router",
    config: () => {
      tanstackRan = true;
      return { environments: { ssr: { dev: { createEnvironment: () => ({}) } } } };
    },
  };
  assert.equal(await detectSsrRunnerBacked({ plugins: [skipped] }), false);
  assert.equal(tanstackRan, false);
  const checker = {
    name: "vite-plugin-checker",
    config: () => ({ environments: { ssr: { dev: { createEnvironment: () => ({}) } } } }),
  };
  assert.equal(await detectSsrRunnerBacked({ plugins: [checker] }), false);

  // A throwing config hook must not fail extraction nor mask other plugins.
  const throwing = { name: "boom", config: () => { throw new Error("boom"); } };
  assert.equal(await detectSsrRunnerBacked({ plugins: [throwing, declaring] }), true);
  assert.equal(await detectSsrRunnerBacked({ plugins: [throwing] }), false);

  // `apply` gates the hook exactly as Vite's filterPlugin does.
  assert.equal(
    await detectSsrRunnerBacked({ plugins: [{ ...declaring, apply: "build" }] }, { command: "serve" }),
    false,
  );
  assert.equal(
    await detectSsrRunnerBacked({ plugins: [{ ...declaring, apply: "build" }] }, { command: "build" }),
    true,
  );

  // No declaration: a plain SSR app is not runner-backed, and a factory-less
  // environments block (a resolved-config shape) cannot false-positive.
  assert.equal(await detectSsrRunnerBacked({ ssr: { target: "node" }, plugins: [{ name: "react" }] }), false);
  assert.equal(await detectSsrRunnerBacked(null), false);
  assert.equal(await detectSsrRunnerBacked({ environments: { ssr: { resolve: { conditions: ["workerd"] } } } }), false);
});

// Vite's getSortedPluginsByHook makes the HOOK's own `order` primary: an
// `order: "pre"` hook splices to the very front of the enforce-sorted list, so
// a plain plugin's order-pre config hook runs BEFORE an enforce-pre plugin's
// plain config hook. Pinned via the successive-merge visibility rule.
test("detectSsrRunnerBacked: hook order is primary, enforce secondary (Vite getSortedPluginsByHook)", async () => {
  const orderPre = {
    name: "plain-plugin-order-pre-hook",
    config: { order: "pre", handler: () => ({ custom: { first: true } }) },
  };
  const enforcePre = {
    name: "enforce-pre-plain-hook",
    enforce: "pre",
    config: (conf) =>
      conf.custom?.first
        ? { environments: { ssr: { dev: { createEnvironment: () => ({}) } } } }
        : null,
  };
  // The enforce-pre plugin sorts earlier by enforce, but its plain hook must
  // still run AFTER the order-pre hook — so it sees the marker and declares.
  assert.equal(await detectSsrRunnerBacked({ plugins: [enforcePre, orderPre] }), true);

  // Inverted expectation guard: were enforce primary, the marker would be
  // invisible and nothing would declare. Prove the sensitivity of the pin.
  const enforcePreFirstSees = {
    name: "sees-nothing",
    enforce: "pre",
    config: (conf) => (conf.custom?.first ? null : { probe: { ranFirst: true } }),
  };
  const late = {
    name: "late-post",
    config: {
      order: "post",
      handler: (conf) =>
        conf.probe?.ranFirst
          ? null
          : { environments: { ssr: { dev: { createEnvironment: () => ({}) } } } },
    },
  };
  // orderPre runs first, then the enforce-pre plain hook (probe NOT set since
  // custom.first is visible), then the order-post hook declares.
  assert.equal(await detectSsrRunnerBacked({ plugins: [enforcePreFirstSees, orderPre, late] }), true);
});

// Vite honors configEnvironment declarations too: runConfigEnvironmentHook
// runs after the config hooks, once per environment name, and merges each
// return into config.environments[name] BEFORE the default factory fill — a
// dev.createEnvironment declared there is a real runner declaration.
test("detectSsrRunnerBacked: configEnvironment-declared factories are seen", async () => {
  const viaEnvHook = {
    name: "acme-workerd",
    configEnvironment: (name) =>
      name === "ssr" ? { dev: { createEnvironment: () => ({}) } } : null,
  };
  assert.equal(await detectSsrRunnerBacked({ plugins: [viaEnvHook] }), true);

  // The hook sees environments the config hooks declared by name, and the
  // implicit client/ssr fill exists for it (Vite fills both before running it).
  const seen = [];
  const recorder = { name: "recorder", configEnvironment: (name) => void seen.push(name) };
  const addsEnv = { name: "adds-env", config: () => ({ environments: { worker: {} } }) };
  assert.equal(await detectSsrRunnerBacked({ plugins: [recorder, addsEnv] }), false);
  assert.deepEqual(seen.sort(), ["client", "ssr", "worker"]);

  // Skip lists and the per-hook guard apply exactly as for config hooks.
  const skipped = {
    name: "tanstack-router",
    configEnvironment: () => ({ dev: { createEnvironment: () => ({}) } }),
  };
  assert.equal(await detectSsrRunnerBacked({ plugins: [skipped] }), false);
  const throwing = { name: "boom-env", configEnvironment: () => { throw new Error("boom"); } };
  assert.equal(await detectSsrRunnerBacked({ plugins: [throwing, viaEnvHook] }), true);

  // Under `command: "build"` with no ssr config there is no implicit ssr
  // environment (Vite only fills it for serve, or when ssr/build.ssr is set).
  const ssrOnly = {
    name: "ssr-only",
    configEnvironment: (name) => (name === "ssr" ? { dev: { createEnvironment: () => ({}) } } : null),
  };
  assert.equal(await detectSsrRunnerBacked({ plugins: [ssrOnly] }, { command: "build" }), false);
  assert.equal(
    await detectSsrRunnerBacked({ ssr: {}, plugins: [ssrOnly] }, { command: "build" }),
    true,
  );
});

// A config hook that calls this.error() fails that plugin's evaluation the way
// a throw does: loud on stderr, naming the plugin, and no declaration from it
// (Vite aborts outright; the extractor deliberately degrades — but never
// silently).
test("detectSsrRunnerBacked: this.error() in a config hook is a named failure, not a silent no-op", async () => {
  const erroring = {
    name: "self-erroring",
    config() {
      this.error("configuration exploded");
    },
  };
  let result;
  const lines = [];
  const write = process.stderr.write;
  process.stderr.write = (chunk) => { lines.push(String(chunk)); return true; };
  try {
    result = await detectSsrRunnerBacked({ plugins: [erroring] });
  } finally {
    process.stderr.write = write;
  }
  assert.equal(result, false);
  const err = lines.join("");
  assert.match(err, /self-erroring/);
  assert.match(err, /configuration exploded/);
});

// The true-wins rule of Vite's mergeConfigRecursively: on ssr/resolve
// noExternal|external a `true` on either side wins over lists instead of
// concatenating into a nonsense array; environments.<name> restarts the path.
test("mergeConfigLite: ssr/resolve noExternal|external true wins over lists", () => {
  assert.deepEqual(
    mergeConfigLite({ ssr: { noExternal: ["a"] } }, { ssr: { noExternal: true } }),
    { ssr: { noExternal: true } },
  );
  assert.deepEqual(
    mergeConfigLite({ ssr: { external: true } }, { ssr: { external: ["b"] } }),
    { ssr: { external: true } },
  );
  assert.deepEqual(
    mergeConfigLite(
      { environments: { ssr: { resolve: { noExternal: true } } } },
      { environments: { ssr: { resolve: { noExternal: ["x"] } } } },
    ),
    { environments: { ssr: { resolve: { noExternal: true } } } },
  );
  // Outside those paths, arrays still concatenate and scalars still replace.
  assert.deepEqual(
    mergeConfigLite({ ssr: { noExternal: ["a"] } }, { ssr: { noExternal: ["b"] } }),
    { ssr: { noExternal: ["a", "b"] } },
  );
  // ... and outside those paths the rule does NOT apply (as in Vite, where
  // the special case is keyed on the ssr/resolve rootPath): arrays concat.
  assert.deepEqual(mergeConfigLite({ other: { external: ["a"] } }, { other: { external: true } }), {
    other: { external: ["a", true] },
  });
});

// The recursion guard is Vite's isObject (Object.prototype.toString ===
// "[object Object]", mirrored exactly), not a bare typeof check: a non-plain
// object (a RegExp, a Date) on both sides must take the later VALUE, not be
// spread key-by-key into a plain `{}` (a RegExp has no own enumerable keys —
// the old guard merged two noExternal RegExps into an empty object).
test("mergeConfigLite: non-plain objects like RegExps replace, never spread", () => {
  const later = /later/;
  const merged = mergeConfigLite({ ssr: { noExternal: /early/ } }, { ssr: { noExternal: later } });
  assert.equal(merged.ssr.noExternal, later, "the later RegExp wins as a value");
  assert.ok(merged.ssr.noExternal instanceof RegExp, "still a RegExp, not {}");
  const date = new Date(0);
  assert.equal(
    mergeConfigLite({ stamp: new Date() }, { stamp: date }).stamp,
    date,
    "a Date replaces instead of merging into a plain object",
  );
  // An array side still concatenates a RegExp value, as in Vite.
  assert.deepEqual(
    mergeConfigLite({ other: { list: ["a"] } }, { other: { list: later } }),
    { other: { list: ["a", later] } },
  );
  // Plain objects still merge recursively.
  assert.deepEqual(
    mergeConfigLite({ resolve: { dedupe: ["a"] } }, { resolve: { conditions: ["c"] } }),
    { resolve: { dedupe: ["a"], conditions: ["c"] } },
  );
});

// The twins ship in two self-contained assets; drift between them is a bug
// class of its own (a fix landing in one file only). Assert byte-identity of
// the whole mergeConfigLite block (environmentPathRE + isPlainObject +
// function) so drift fails CI instead of shipping.
test("mergeConfigLite twins are byte-identical across the two shipped assets", () => {
  const block = (rel) => {
    const src = fs.readFileSync(asset(rel), "utf8");
    const m = src.match(
      /const environmentPathRE = [\s\S]*?\nfunction mergeConfigLite\(defaults, overrides, rootPath = ""\) \{[\s\S]*?\n\}\n/,
    );
    assert.ok(m, `${rel}: mergeConfigLite block not found`);
    return m[0];
  };
  assert.equal(
    block("vite-extract.mjs"),
    block("plugin-host.mjs"),
    "the two mergeConfigLite copies drifted — keep them byte-identical",
  );
});

// Same drift guard for the other twins the two assets carry: the NODE_ENV
// rule (the extractor runs it per call against an isolated process.env on the
// embedded engine while the plugin host still runs it at module scope in its
// own node process, so only the rule's SEMANTIC lines are shared now: the
// command-derived default and the conditional unset) and the externalize-deps
// config-bundling plugin.
test("NODE_ENV-rule and externalizeDepsPlugin twins are byte-identical across the two shipped assets", () => {
  const block = (rel, re, what) => {
    const m = fs.readFileSync(asset(rel), "utf8").match(re);
    assert.ok(m, `${rel}: ${what} block not found`);
    return m[0];
  };
  const defaultRuleRE = /command === "build" \? "production" : "development"/;
  block("vite-extract.mjs", defaultRuleRE, "command-derived NODE_ENV default");
  block("plugin-host.mjs", defaultRuleRE, "command-derived NODE_ENV default");
  const unsetRE = /const unsetOjNodeEnvForResolve = \(\) => \{\n[\s\S]*?\n\};\n/;
  assert.equal(
    block("vite-extract.mjs", unsetRE, "NODE_ENV unset rule"),
    block("plugin-host.mjs", unsetRE, "NODE_ENV unset rule"),
    "the two NODE_ENV unset-rule copies drifted — keep them byte-identical",
  );
  const extRE = /\nfunction externalizeDepsPlugin\(\) \{\n[\s\S]*?\n\}\n/;
  assert.equal(
    block("vite-extract.mjs", extRE, "externalizeDepsPlugin"),
    block("plugin-host.mjs", extRE, "externalizeDepsPlugin"),
    "the two externalizeDepsPlugin copies drifted — keep them byte-identical",
  );
});
