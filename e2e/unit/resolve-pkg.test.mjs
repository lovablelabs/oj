// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, realpathSync, rmSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { viteEnvDefine, envPrefixes, environmentDefines, makeResolver, importPkg, resolveOjRolldown, ssrExternalRule } from "../../crates/oj_server/src/assets/start/resolve-pkg.mjs";

test("viteEnvDefine builds import.meta.env with the standard flags", () => {
  process.env.VITE_ONLY_FOR_TEST = "hello";
  try {
    const def = viteEnvDefine({ ssr: false, mode: "development" });
    const env = JSON.parse(def["import.meta.env"]);
    assert.equal(env.MODE, "development");
    assert.equal(env.DEV, true);
    assert.equal(env.PROD, false);
    assert.equal(env.SSR, false);
    assert.equal(env.BASE_URL, "/");
    assert.equal(env.VITE_ONLY_FOR_TEST, "hello");
  } finally {
    delete process.env.VITE_ONLY_FOR_TEST;
  }
});

test("viteEnvDefine reflects ssr and production", () => {
  const env = JSON.parse(viteEnvDefine({ ssr: true, mode: "production" })["import.meta.env"]);
  assert.equal(env.SSR, true);
  assert.equal(env.MODE, "production");
  assert.equal(env.PROD, true);
  assert.equal(env.DEV, false);
});

// Vite: `envPrefix` decides which vars reach import.meta.env; DEV/PROD follow
// NODE_ENV even in a development mode (`NODE_ENV=production vite dev` is PROD).
test("viteEnvDefine honors envPrefix (OJ_ENV_PREFIX) and NODE_ENV over mode", () => {
  const source = { NODE_ENV: "production", VITE_A: "a", APP_B: "b", SECRET_C: "c", OJ_ENV_PREFIX: JSON.stringify(["VITE_", "APP_"]) };
  assert.deepEqual(envPrefixes(source), ["VITE_", "APP_"]);
  assert.deepEqual(envPrefixes({}), ["VITE_"]);
  assert.deepEqual(envPrefixes({ OJ_ENV_PREFIX: "not json" }), ["VITE_"]);
  const env = JSON.parse(viteEnvDefine({ mode: "development", env: source, prefixes: envPrefixes(source) })["import.meta.env"]);
  assert.equal(env.MODE, "development");
  assert.equal(env.DEV, false);
  assert.equal(env.PROD, true);
  assert.equal(env.VITE_A, "a");
  assert.equal(env.APP_B, "b");
  assert.equal("SECRET_C" in env, false);
  const only = JSON.parse(viteEnvDefine({ env: source, prefixes: ["APP_"] })["import.meta.env"]);
  assert.equal("VITE_A" in only, false);
  assert.equal(only.APP_B, "b");
});

test("environmentDefines reads the per-environment define maps oj hands over", () => {
  const source = { OJ_DEFINE_CLIENT: JSON.stringify({ __SIDE__: '"client"' }), OJ_DEFINE_SSR: JSON.stringify({ __SIDE__: '"server"' }) };
  assert.deepEqual(environmentDefines("client", source), { __SIDE__: '"client"' });
  assert.deepEqual(environmentDefines("ssr", source), { __SIDE__: '"server"' });
  assert.deepEqual(environmentDefines("client", {}), {});
  assert.deepEqual(environmentDefines("ssr", { OJ_DEFINE_SSR: "{broken" }), {});
});

function pkg(dir, name, main = "index.js") {
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, "package.json"), JSON.stringify({ name, main, version: "1.0.0" }));
  writeFileSync(join(dir, main), "module.exports = {};");
}

test("makeResolver resolves a direct dep from the app root", () => {
  const root = mkdtempSync(join(tmpdir(), "oj-resolve-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app"}');
    pkg(join(root, "node_modules", "foo"), "foo");
    const resolve = makeResolver(root);
    assert.match(resolve("foo"), /node_modules[/\\]foo[/\\]index\.js$/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("makeResolver reaches a transitive dep through a direct-dep anchor", () => {
  const root = mkdtempSync(join(tmpdir(), "oj-resolve-anchor-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app"}');
    pkg(join(root, "node_modules", "anchor"), "anchor");
    pkg(join(root, "node_modules", "anchor", "node_modules", "dep"), "dep");
    const resolve = makeResolver(root);
    assert.throws(() => resolve("dep"));
    assert.match(resolve("dep", ["anchor"]), /anchor[/\\]node_modules[/\\]dep[/\\]index\.js$/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("makeResolver reaches a dep two hops deep (transitive through a transitive)", () => {
  // Mirrors the real TanStack Start layout under pnpm:
  //   app -> @tanstack/react-start -> @tanstack/start-plugin-core -> @tanstack/router-generator
  // The generator is two hops from the app root, resolvable only by walking the
  // dependency graph, not by anchoring a single level down. The old single-hop
  // resolver threw here; the breadth-first walk finds it.
  const root = mkdtempSync(join(tmpdir(), "oj-resolve-2hop-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app","dependencies":{"anchor":"1.0.0"}}');
    const anchorDir = join(root, "node_modules", "anchor");
    mkdirSync(anchorDir, { recursive: true });
    writeFileSync(join(anchorDir, "package.json"), '{"name":"anchor","main":"index.js","dependencies":{"mid":"1.0.0"}}');
    writeFileSync(join(anchorDir, "index.js"), "module.exports = {};");
    const midDir = join(anchorDir, "node_modules", "mid");
    mkdirSync(midDir, { recursive: true });
    writeFileSync(join(midDir, "package.json"), '{"name":"mid","main":"index.js","dependencies":{"deep":"1.0.0"}}');
    writeFileSync(join(midDir, "index.js"), "module.exports = {};");
    pkg(join(midDir, "node_modules", "deep"), "deep");
    const resolve = makeResolver(root);
    assert.match(resolve("deep"), /mid[/\\]node_modules[/\\]deep[/\\]index\.js$/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("makeResolver walks past an intermediate whose exports omit ./package.json", () => {
  // Some packages expose an exports map that does not list "./package.json", so
  // "<name>/package.json" is not resolvable; the walk must still find the
  // package's own manifest to read its dependencies and continue.
  const root = mkdtempSync(join(tmpdir(), "oj-resolve-exports-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app","dependencies":{"anchor":"1.0.0"}}');
    const anchorDir = join(root, "node_modules", "anchor");
    mkdirSync(anchorDir, { recursive: true });
    writeFileSync(
      join(anchorDir, "package.json"),
      '{"name":"anchor","exports":{".":"./index.js"},"dependencies":{"deep":"1.0.0"}}',
    );
    writeFileSync(join(anchorDir, "index.js"), "module.exports = {};");
    pkg(join(anchorDir, "node_modules", "deep"), "deep");
    assert.throws(() => makeResolver(root)("anchor/package.json"));
    assert.match(makeResolver(root)("deep"), /anchor[/\\]node_modules[/\\]deep[/\\]index\.js$/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("makeResolver throws a clear error for a missing package", () => {
  const root = mkdtempSync(join(tmpdir(), "oj-resolve-miss-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app"}');
    const resolve = makeResolver(root);
    assert.throws(() => resolve("does-not-exist"), /cannot resolve/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

function writePkg(root, name, files) {
  const dir = join(root, "node_modules", name);
  mkdirSync(dir, { recursive: true });
  for (const [f, contents] of Object.entries(files)) writeFileSync(join(dir, f), contents);
}

test("importPkg imports a CJS package and unwraps module.exports", async () => {
  const root = mkdtempSync(join(tmpdir(), "oj-import-cjs-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app"}');
    writePkg(root, "cjspkg", {
      "package.json": '{"name":"cjspkg","main":"index.js"}',
      "index.js": 'module.exports = { version: "1.0", build: () => "built" };',
    });
    const mod = await importPkg(root, "cjspkg");
    assert.equal(mod.version, "1.0");
    assert.equal(typeof mod.build, "function");
    assert.equal(mod.build(), "built");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("importPkg imports an ESM package and returns its namespace", async () => {
  const root = mkdtempSync(join(tmpdir(), "oj-import-esm-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app"}');
    writePkg(root, "esmpkg", {
      "package.json": '{"name":"esmpkg","main":"index.mjs"}',
      "index.mjs": 'export const value = 42; export function greet() { return "hi"; }',
    });
    const mod = await importPkg(root, "esmpkg");
    assert.equal(mod.value, 42);
    assert.equal(mod.greet(), "hi");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("importPkg reaches a transitive dep through a preferred anchor", async () => {
  const root = mkdtempSync(join(tmpdir(), "oj-import-anchor-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app"}');
    writePkg(root, "anchor", { "package.json": '{"name":"anchor"}' });
    writePkg(root, join("anchor", "node_modules", "dep"), {
      "package.json": '{"name":"dep","main":"index.js"}',
      "index.js": 'module.exports = { ok: true };',
    });
    const mod = await importPkg(root, "dep", ["anchor"]);
    assert.equal(mod.ok, true);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("tailwind engine module resolves Tailwind v4 dependencies beneath the Vite plugin", async () => {
  const root = mkdtempSync(join(tmpdir(), "oj-tailwind-strict-layout-"));
  try {
    writeFileSync(join(root, "package.json"), JSON.stringify({
      name: "synthetic-app",
      dependencies: { "@tailwindcss/vite": "1.0.0" },
    }));
    writePkg(root, "@tailwindcss/vite", {
      "package.json": JSON.stringify({
        name: "@tailwindcss/vite",
        main: "index.js",
        dependencies: { "@tailwindcss/node": "1.0.0", "@tailwindcss/oxide": "1.0.0" },
      }),
      "index.js": "module.exports = {};",
    });
    const anchor = join(root, "node_modules", "@tailwindcss", "vite");
    writePkg(anchor, "@tailwindcss/node", {
      "package.json": '{"name":"@tailwindcss/node","type":"module","main":"index.mjs"}',
      "index.mjs": "export async function compile(source) { return { build(tokens) { return source + tokens.join(','); } }; }",
    });
    writePkg(anchor, "@tailwindcss/oxide", {
      "package.json": '{"name":"@tailwindcss/oxide","type":"module","main":"index.mjs"}',
      "index.mjs": "export class Scanner { scan() { return ['synthetic-tailwind-token']; } }",
    });

    const requireFromApp = createRequire(join(root, "package.json"));
    assert.throws(() => requireFromApp.resolve("@tailwindcss/node"), { code: "MODULE_NOT_FOUND" });
    assert.throws(() => requireFromApp.resolve("@tailwindcss/oxide"), { code: "MODULE_NOT_FOUND" });

    const stylesheet = join(root, "styles.css");
    writeFileSync(stylesheet, '@import "tailwindcss";');
    const { compile } = await import(
      new URL("../../crates/oj_server/src/assets/css-tailwind.mjs", import.meta.url).href
    );
    const css = await compile({ base: root, css: '@import "tailwindcss";', from: stylesheet });
    assert.equal(css, '@import "tailwindcss";synthetic-tailwind-token');
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("importPkg rejects when the package cannot be resolved", async () => {
  const root = mkdtempSync(join(tmpdir(), "oj-import-miss-"));
  try {
    writeFileSync(join(root, "package.json"), '{"name":"app"}');
    await assert.rejects(importPkg(root, "not-installed"), /cannot resolve/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("ssrExternalRule keeps explicit ssr.external deps out of the Start server bundle", () => {
  const root = mkdtempSync(join(tmpdir(), "oj-ssr-external-"));
  try {
    mkdirSync(join(root, "node_modules", "installed"), { recursive: true });
    writeFileSync(join(root, "node_modules", "installed", "package.json"), '{"name":"installed"}');
    const none = ssrExternalRule(root, {});
    assert.equal(none("react", undefined, false), false, "no config: everything is bundled");

    const listed = ssrExternalRule(root, { OJ_SSR_EXTERNALS: JSON.stringify({ external: ["react", "@scope/pkg"] }) });
    assert.equal(listed("react", undefined, false), true);
    assert.equal(listed("react/jsx-runtime", undefined, false), true, "subpaths follow the package");
    assert.equal(listed("@scope/pkg/deep", undefined, false), true);
    assert.equal(listed("@scope/other", undefined, false), false);
    assert.equal(listed("./react", undefined, false), false, "relative ids are never external");
    assert.equal(listed("\0virtual:react", undefined, false), false);
    assert.equal(listed("/abs/node_modules/react/index.js", undefined, true), false, "resolved paths are left to the bundle");
    assert.equal(listed("installed", undefined, false), false, "an installed dep not listed is bundled");

    const all = ssrExternalRule(root, { OJ_SSR_EXTERNALS: JSON.stringify({ externalAll: true, external: [] }) });
    assert.equal(all("installed", undefined, false), true, "external: true externalizes installed deps");
    assert.equal(all("#alias-looking-bare", undefined, false), false, "an alias that looks bare is bundled");
    assert.equal(all("not-installed", undefined, false), false);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

// The Start bundles are oj's own code: rolldown resolves to the copy
// vendored next to the oj binary (OJ_VENDORED_ROLLDOWN) ahead of whatever
// the app pins — a vite <= 7 app pins none at all — and the app's copy is
// the fallback when no vendor exists.
function fakePkg(dir, name) {
  const root = join(dir, "node_modules", name);
  mkdirSync(root, { recursive: true });
  writeFileSync(join(root, "package.json"), JSON.stringify({ name, version: "0.0.1", main: "index.cjs" }));
  writeFileSync(join(root, "index.cjs"), `module.exports = { from: ${JSON.stringify(dir)} };`);
  return root;
}

test("resolveOjRolldown prefers the vendored copy and falls back to the app's own", () => {
  const app = mkdtempSync(join(tmpdir(), "oj-vendor-app-"));
  const vendor = mkdtempSync(join(tmpdir(), "oj-vendor-dir-"));
  writeFileSync(join(app, "package.json"), JSON.stringify({ name: "app", version: "1.0.0" }));
  fakePkg(vendor, "rolldown");
  const own = fakePkg(app, "rolldown");
  const prior = process.env.OJ_VENDORED_ROLLDOWN;
  let scratchBroken;
  try {
    process.env.OJ_VENDORED_ROLLDOWN = vendor;
    const resolved = resolveOjRolldown(app);
    // realpath both sides: macOS tmpdir is a /private symlink.
    assert.ok(realpathSync(resolved).startsWith(realpathSync(vendor)), `vendored copy expected, got ${resolved}`);
    // The vendor is oj's own tooling preference: the app's module-graph
    // resolution (makeResolver) keeps the lockfile's copy, never the vendor's.
    assert.ok(realpathSync(makeResolver(app)("rolldown")).startsWith(realpathSync(own)));
    // Set-but-empty is the explicit opt-out; the app's own copy serves.
    process.env.OJ_VENDORED_ROLLDOWN = "";
    assert.ok(realpathSync(resolveOjRolldown(app)).startsWith(realpathSync(own)));
    // A vendor that cannot resolve rolldown falls back to the app's copy
    // (an isolated dir: node's lookup walks up, so a subdir of the vendor
    // would still find the vendor's own node_modules).
    process.env.OJ_VENDORED_ROLLDOWN = mkdtempSync(join(tmpdir(), "oj-broken-vendor-"));
    scratchBroken = process.env.OJ_VENDORED_ROLLDOWN;
    assert.ok(realpathSync(resolveOjRolldown(app)).startsWith(realpathSync(own)));
    // Without a vendor, the app's own copy still serves.
    delete process.env.OJ_VENDORED_ROLLDOWN;
    assert.ok(realpathSync(resolveOjRolldown(app)).startsWith(realpathSync(own)));
  } finally {
    if (prior === undefined) delete process.env.OJ_VENDORED_ROLLDOWN;
    else process.env.OJ_VENDORED_ROLLDOWN = prior;
    rmSync(app, { recursive: true, force: true });
    rmSync(vendor, { recursive: true, force: true });
    if (scratchBroken) rmSync(scratchBroken, { recursive: true, force: true });
  }
});

test("a missing rolldown names the fallback, or the broken vendor, in its error", () => {
  const app = mkdtempSync(join(tmpdir(), "oj-novendor-app-"));
  writeFileSync(join(app, "package.json"), JSON.stringify({ name: "app", version: "1.0.0" }));
  const prior = process.env.OJ_VENDORED_ROLLDOWN;
  try {
    delete process.env.OJ_VENDORED_ROLLDOWN;
    assert.throws(() => resolveOjRolldown(app), /OJ_VENDORED_ROLLDOWN/);
    // A configured-but-unreadable vendor is named, not reported as absent.
    const broken = join(app, "no-such-vendor");
    process.env.OJ_VENDORED_ROLLDOWN = broken;
    assert.throws(
      () => resolveOjRolldown(app),
      (err) => err.message.includes(broken) && !err.message.includes("vendors none"),
    );
  } finally {
    if (prior === undefined) delete process.env.OJ_VENDORED_ROLLDOWN;
    else process.env.OJ_VENDORED_ROLLDOWN = prior;
    rmSync(app, { recursive: true, force: true });
  }
});
