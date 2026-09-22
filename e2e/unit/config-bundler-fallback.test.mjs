// SPDX-License-Identifier: MIT

// The direct config-bundling fallback and its error precedence (#215).
//
// Stock Vite bundles vite.config with esbuild, rolldown-vite with rolldown,
// and an app only ships the bundler its vite uses. The fallback used to
// require.resolve esbuild unconditionally, so on a rolldown-vite app it died
// with "Cannot find module 'esbuild'" — and because the fallback also ran
// when the config's OWN code had thrown inside vite.loadConfigFromFile, that
// bundler error REPLACED the config's real error. Now:
//   - the fallback bundles with the bundler the app's vite itself declares
//     (rolldown-vite declares rolldown), falling back to whichever resolves;
//   - an error thrown while vite's own loader ran the config propagates
//     as-is in BOTH loaders (re-bundling re-runs the same code into the same
//     error), exactly as Vite's loadConfigFromFile rethrows;
//   - with no vite and no bundler, the error says what is missing.

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import {
  bootHost,
  configMonorepoFixture,
  linkRolldown,
  runExtract,
  testWithRolldown,
  tmpProject,
} from "./harness.mjs";

const testRolldown = testWithRolldown(test);

const rolldownMonorepoFixture = () =>
  configMonorepoFixture({ prefix: "oj-cfg-rd-", bundler: "rolldown" });

testRolldown("vite-extract bundles a TS config with rolldown when esbuild is absent", () => {
  const fx = rolldownMonorepoFixture();
  try {
    const { json, stderr } = runExtract(fx, { prefix: "oj-cfg-rd-run-" });
    assert.equal(json.__ok, true, `extraction failed, got: ${JSON.stringify(json)}\nstderr:\n${stderr}`);
    assert.equal(json.base, "/from-pkg-dep/", "base set from the constant only pkg/node_modules provides");
    assert.doesNotMatch(stderr, /Cannot find module 'esbuild'/);
    // The inlined first-party TS file is a config dependency (rolldown
    // moduleIds), so editing it restarts the dev server like the config.
    const plugin = path.join(fx.base, "pkg", "src", "plugin.ts");
    assert.ok(
      json.__deps.some((d) => fs.existsSync(d) && fs.realpathSync(d) === fs.realpathSync(plugin)),
      `__deps must include the inlined sibling source; got: ${JSON.stringify(json.__deps)}`,
    );
  } finally {
    fx.cleanup();
  }
});

testRolldown("the plugin host bundles the same config with rolldown and the ../pkg plugin is active", async () => {
  const fx = rolldownMonorepoFixture();
  const { host, cleanup } = bootHost(fx, { prefix: "oj-cfg-rd-host-" });
  try {
    const count = await host.send({ id: 1, hook: "getPluginCount", args: [] });
    assert.equal(count.result, "1", `plugin from ../pkg did not load; stderr:\n${host.stderr()}`);
    const res = await host.send({ id: 2, hook: "getPluginConfig", args: [] });
    const cfg = JSON.parse(res.result);
    assert.equal(cfg.define?.__FROM_PKG__, '"/from-pkg-dep/"', "config() hook ran with the dep's value");
    assert.doesNotMatch(host.stderr(), /Cannot find module 'esbuild'/);
  } finally {
    host.close();
    cleanup();
    fx.cleanup();
  }
});

testRolldown("rolldown inject-file-scope: __dirname/__filename/import.meta.url stay per-file originals", async () => {
  // The define map rewrites __dirname & co to __vite_injected_original_*
  // and the transform hook prepends each file's own consts. If the define
  // were ignored, the ESM bundle would hit a bare __dirname and the config
  // would fail to load; if the consts were not scoped per module, the
  // sibling file would see the config's paths instead of its own.
  const fx = tmpProject({ prefix: "oj-cfg-scope-" });
  linkRolldown(fx.root);
  fx.write("app/package.json", JSON.stringify({ name: "app", type: "module" }));
  fx.write(
    "app/vite.config.ts",
    `import { pkgProbe } from "../pkg/src/probe";
export default {
  plugins: [{
    name: "scope-probe",
    config() {
      return { define: {
        __CFG_DIRNAME__: JSON.stringify(__dirname),
        __CFG_FILENAME__: JSON.stringify(__filename),
        __CFG_URL__: JSON.stringify(import.meta.url),
        __PKG_DIRNAME__: JSON.stringify(pkgProbe),
      } };
    },
  }],
};
`,
  );
  fx.write("pkg/package.json", JSON.stringify({ name: "pkg", type: "module" }));
  fx.write("pkg/src/probe.ts", `export const pkgProbe: string = __dirname;\n`);
  const appFx = { appRoot: path.join(fx.root, "app"), configPath: path.join(fx.root, "app", "vite.config.ts") };
  const { host, cleanup } = bootHost(appFx, { prefix: "oj-cfg-scope-host-" });
  try {
    const res = await host.send({ id: 1, hook: "getPluginConfig", args: [] });
    assert.ok(res.result, `config() never ran; stderr:\n${host.stderr()}`);
    const define = JSON.parse(res.result).define ?? {};
    const appDir = fs.realpathSync(appFx.appRoot);
    const norm = (v) => fs.realpathSync(JSON.parse(v));
    assert.equal(norm(define.__CFG_DIRNAME__), appDir, "__dirname is the config's original dir");
    assert.equal(
      norm(define.__CFG_FILENAME__),
      fs.realpathSync(appFx.configPath),
      "__filename is the config's original path",
    );
    assert.ok(
      JSON.parse(define.__CFG_URL__).endsWith("/app/vite.config.ts"),
      `import.meta.url points at the original file, got ${define.__CFG_URL__}`,
    );
    assert.equal(
      norm(define.__PKG_DIRNAME__),
      fs.realpathSync(path.join(fx.root, "pkg", "src")),
      "the inlined sibling file keeps ITS OWN __dirname (per-module scoping)",
    );
  } finally {
    host.close();
    cleanup();
    fx.cleanup();
  }
});

testRolldown("rolldown bundles a dynamic import into the single chunk (codeSplitting off)", async () => {
  // Vite generates one chunk "like esbuild does with splitting: false"; a
  // second chunk would be a sibling file the tmp-bundle import could never
  // find. A dynamically imported first-party TS module must be inlined.
  const fx = rolldownMonorepoFixture();
  fx.write(
    "app/vite.config.ts",
    `const mod = await import("../pkg/src/plugin");
export default {
  base: mod.ojBase,
  plugins: [mod.makePlugin()],
};
`,
  );
  const { host, cleanup } = bootHost(fx, { prefix: "oj-cfg-dyn-host-" });
  try {
    const count = await host.send({ id: 1, hook: "getPluginCount", args: [] });
    assert.equal(count.result, "1", `dynamically imported plugin did not load; stderr:\n${host.stderr()}`);
    const res = await host.send({ id: 2, hook: "getPluginConfig", args: [] });
    assert.equal(JSON.parse(res.result).define?.__FROM_PKG__, '"/from-pkg-dep/"');
  } finally {
    host.close();
    cleanup();
    fx.cleanup();
  }
});

testRolldown("rolldown nested under the vite package resolves (a vite without loadConfigFromFile falls through)", async () => {
  // The unhoisted shape: rolldown is vite's dependency, not the app's, so the
  // fallback's resolver must reach node_modules/vite/node_modules/rolldown.
  // The stub vite has no loadConfigFromFile, exercising the same fall-through
  // an old vite takes.
  const fx = tmpProject({ prefix: "oj-cfg-nested-" });
  fx.pkg("vite", "index.mjs", { "index.mjs": `export const version = "0.0.0-stub";\n` });
  linkRolldown(path.join(fx.root, "node_modules", "vite"));
  fx.write(
    "vite.config.ts",
    `const marker: string = "/nested-rolldown/";
export default {
  plugins: [{
    name: "nested-probe",
    config() {
      return { define: { __NESTED__: JSON.stringify(marker) } };
    },
  }],
};
`,
  );
  const appFx = { appRoot: fx.root, configPath: path.join(fx.root, "vite.config.ts") };
  const { host, cleanup } = bootHost(appFx, { prefix: "oj-cfg-nested-host-" });
  try {
    const count = await host.send({ id: 1, hook: "getPluginCount", args: [] });
    assert.equal(count.result, "1", `TS config did not load; stderr:\n${host.stderr()}`);
    const res = await host.send({ id: 2, hook: "getPluginConfig", args: [] });
    assert.equal(JSON.parse(res.result).define?.__NESTED__, '"/nested-rolldown/"');
    assert.doesNotMatch(host.stderr(), /Cannot find module/);
  } finally {
    host.close();
    cleanup();
    fx.cleanup();
  }
});

testRolldown("the bundler vite declares wins over a hoisted impostor", async () => {
  // A rolldown-vite app with a (broken, here) esbuild hoisted next to it —
  // mid-migration trees and tsx/vitest installs produce exactly this. The
  // selection must ask "what does this vite bundle configs with?" (its own
  // dependencies), not "what is installed?": picking the hoisted esbuild
  // would bundle the config differently than the app's own `vite dev`.
  const fx = tmpProject({ prefix: "oj-cfg-prefer-" });
  fx.pkg("vite", "index.mjs", { "index.mjs": `export const version = "0.0.0-stub";\n` });
  const viteDir = path.join(fx.root, "node_modules", "vite");
  fs.writeFileSync(
    path.join(viteDir, "package.json"),
    JSON.stringify({ name: "vite", version: "0.0.0-stub", main: "index.mjs", dependencies: { rolldown: "*" } }),
  );
  linkRolldown(viteDir);
  fx.pkg("esbuild", "index.js", { "index.js": `throw new Error("the hoisted esbuild impostor was chosen");\n` });
  fx.write(
    "vite.config.ts",
    `const marker: string = "/prefer-rolldown/";
export default {
  plugins: [{
    name: "prefer-probe",
    config() {
      return { define: { __PREFER__: JSON.stringify(marker) } };
    },
  }],
};
`,
  );
  const appFx = { appRoot: fx.root, configPath: path.join(fx.root, "vite.config.ts") };
  const { host, cleanup } = bootHost(appFx, { prefix: "oj-cfg-prefer-host-" });
  try {
    const count = await host.send({ id: 1, hook: "getPluginCount", args: [] });
    assert.equal(count.result, "1", `TS config did not load; stderr:\n${host.stderr()}`);
    const res = await host.send({ id: 2, hook: "getPluginConfig", args: [] });
    assert.equal(JSON.parse(res.result).define?.__PREFER__, '"/prefer-rolldown/"');
    assert.doesNotMatch(host.stderr(), /impostor was chosen/);
  } finally {
    host.close();
    cleanup();
    fx.cleanup();
  }
});

test("an error thrown while vite's own loader ran the config propagates un-masked", async () => {
  // The incident shape: vite IS present, its loadConfigFromFile runs the
  // config, and the config's own top-level code throws (a secrets loader, a
  // missing credential). No bundler is installed — the old fallback replaced
  // the real error with "Cannot find module 'esbuild'".
  const fx = tmpProject({ prefix: "oj-cfg-prop-" });
  fx.pkg("vite", "index.mjs", {
    "index.mjs": `export async function loadConfigFromFile() {
  throw new Error("secrets loader exploded (the config's own failure)");
}
`,
  });
  fx.write("vite.config.ts", `export default { base: "/never-loads/" };\n`);
  const appFx = { appRoot: fx.root, configPath: path.join(fx.root, "vite.config.ts") };
  const { host, cleanup } = bootHost(appFx, { prefix: "oj-cfg-prop-host-" });
  try {
    const count = await host.send({ id: 1, hook: "getPluginCount", args: [] });
    assert.equal(count.result, "0", "the host degrades to zero plugins");
    assert.match(
      host.stderr(),
      /failed to load .*vite\.config\.ts: .*secrets loader exploded/,
      `the config's own error must surface; stderr:\n${host.stderr()}`,
    );
    assert.doesNotMatch(host.stderr(), /Cannot find module 'esbuild'/);
    assert.doesNotMatch(host.stderr(), /bundling config directly/);
  } finally {
    host.close();
    cleanup();
    fx.cleanup();
  }
});

test("the extractor propagates a config-execution error the same way (no re-bundle, no masking)", () => {
  // Same incident shape through vite-extract: the two loaders must degrade
  // IDENTICALLY, or extraction hands the dev server a verdict the plugin
  // host can never match (a half-configured server). The extractor reports
  // __ok: false with the config's own error on stderr — not a re-bundled
  // divergent success, and not "no vite, esbuild, or rolldown".
  const fx = tmpProject({ prefix: "oj-cfg-xprop-" });
  fx.pkg("vite", "index.mjs", {
    "index.mjs": `export async function loadConfigFromFile() {
  throw new Error("secrets loader exploded (the config's own failure)");
}
`,
  });
  fx.write("vite.config.ts", `export default { base: "/never-loads/" };\n`);
  const appFx = { appRoot: fx.root, configPath: path.join(fx.root, "vite.config.ts") };
  const { json, stderr } = runExtract(appFx, { prefix: "oj-cfg-xprop-run-" });
  try {
    assert.equal(json.__ok, false, `extraction must fail with the config, got: ${JSON.stringify(json)}`);
    assert.match(stderr, /secrets loader exploded/, `the config's own error must surface; stderr:\n${stderr}`);
    assert.doesNotMatch(stderr, /no vite, esbuild, or rolldown/);
  } finally {
    fx.cleanup();
  }
});

test("with no vite and no bundler, the fallback names what is missing", async () => {
  const fx = tmpProject({ prefix: "oj-cfg-none-" });
  fx.write("vite.config.ts", `export default { base: "/never-loads/" };\n`);
  const appFx = { appRoot: fx.root, configPath: path.join(fx.root, "vite.config.ts") };
  const { host, cleanup } = bootHost(appFx, { prefix: "oj-cfg-none-host-" });
  try {
    const count = await host.send({ id: 1, hook: "getPluginCount", args: [] });
    assert.equal(count.result, "0", "the host degrades to zero plugins");
    // The unavailability note carries the resolver's multi-line require
    // stack, so match the two halves separately.
    assert.match(host.stderr(), /vite unavailable \(/);
    assert.match(host.stderr(), /bundling config directly/);
    assert.match(
      host.stderr(),
      /neither esbuild nor rolldown is installed/,
      `the error must name the missing bundlers; stderr:\n${host.stderr()}`,
    );
  } finally {
    host.close();
    cleanup();
    fx.cleanup();
  }
});
