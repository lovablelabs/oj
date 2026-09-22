// SPDX-License-Identifier: MIT

// A TS vite.config bundled with `packages: "external"` keeps bare specifiers
// bare, and the bundle is imported from the cache dir, so they re-resolve from
// THERE at import time. The monorepo shape breaks it: the config relatively
// imports a sibling workspace package's source (which gets inlined), and that
// source's bare deps live under the sibling's own node_modules -- unreachable
// from the cache dir ("Cannot find package 'picomatch' imported from
// .../.oj-cache/v1/oj-vite-config-....tmp.mjs"). A present-but-unloadable
// vite.config is a hard error, so `oj dev` dies (issue #146's shape).
//
// The fix mirrors Vite's `externalize-deps` plugin in both config bundlers:
// every bare import is resolved from its importer at bundle time and the
// resolved absolute path is externalized instead -- except first-party sources
// (TS, or anything outside node_modules, e.g. a tsconfig-paths alias) and
// .json, which stay bundled -- with Vite's `inject-file-scope-variables`
// keeping `__dirname` / `__filename` / `import.meta.url` pointing at each
// file's ORIGINAL location.

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { asset, bootHost, configMonorepoFixture, runExtract, testWithEsbuild } from "./harness.mjs";

const testEsbuild = testWithEsbuild(test);

const monorepoFixture = () => configMonorepoFixture({ prefix: "oj-cfg-ext-", bundler: "esbuild" });

testEsbuild("vite-extract's esbuild fallback loads a config importing a sibling package's source", () => {
  const fx = monorepoFixture();
  try {
    const { json } = runExtract(fx);
    assert.equal(json.__ok, true, `extraction failed, got: ${JSON.stringify(json)}`);
    assert.equal(json.base, "/from-pkg-dep/", "base set from the constant only pkg/node_modules provides");
  } finally {
    fx.cleanup();
  }
});

testEsbuild("the plugin host loads the same config and the ../pkg plugin is active", async () => {
  const fx = monorepoFixture();
  const { host, cleanup } = bootHost(fx, { prefix: "oj-cfg-ext-host-" });
  try {
    const count = await host.send({ id: 1, hook: "getPluginCount", args: [] });
    assert.equal(count.result, "1", `plugin from ../pkg did not load; stderr:\n${host.stderr()}`);
    const res = await host.send({ id: 2, hook: "getPluginConfig", args: [] });
    const cfg = JSON.parse(res.result);
    assert.equal(cfg.define?.__FROM_PKG__, '"/from-pkg-dep/"', "config() hook ran with the dep's value");
    const leaked = fs
      .readdirSync(path.dirname(asset("plugin-host.mjs")))
      .filter((f) => f.startsWith("oj-vite-config-"));
    assert.deepEqual(leaked, [], "no tmp config bundle may land in the assets dir");
  } finally {
    host.close();
    cleanup();
    fx.cleanup();
  }
});

testEsbuild("a tsconfig-paths-aliased TS import is bundled, runs, and is watched", () => {
  const fx = monorepoFixture();
  try {
    fx.write(
      "app/tsconfig.json",
      JSON.stringify({ compilerOptions: { baseUrl: ".", paths: { "@/*": ["./src/*"] } } }),
    );
    // An enum proves the file was transpiled, not just re-exported: Node could
    // never import the raw .ts.
    fx.write(
      "app/src/thing.ts",
      `export enum Kind { Aliased = "/from-aliased-ts/" }
export const aliasedBase: string = Kind.Aliased;
`,
    );
    fx.write(
      "app/vite.config.ts",
      `import { aliasedBase } from "@/thing";
export default { base: aliasedBase };
`,
    );
    const { json } = runExtract(fx);
    assert.equal(json.__ok, true, `extraction failed, got: ${JSON.stringify(json)}`);
    assert.equal(json.base, "/from-aliased-ts/", "the aliased TS module was inlined and ran");
    // The inlined first-party file is a config dependency: editing it must
    // restart the dev server, so it has to surface in __deps (metafile.inputs).
    const thing = path.join(fx.appRoot, "src", "thing.ts");
    assert.ok(
      json.__deps.some((d) => fs.existsSync(d) && fs.realpathSync(d) === fs.realpathSync(thing)),
      `__deps must include the aliased source; got: ${JSON.stringify(json.__deps)}`,
    );
  } finally {
    fx.cleanup();
  }
});

testEsbuild("an externalized package resolves like Node: import condition, not module", () => {
  const fx = monorepoFixture();
  try {
    fx.write(
      "app/node_modules/cond-pkg/package.json",
      JSON.stringify({
        name: "cond-pkg",
        version: "1.0.0",
        main: "dist/main.cjs",
        module: "dist/module.mjs",
        exports: {
          ".": {
            module: "./dist/module.mjs",
            import: "./dist/import.mjs",
            require: "./dist/require.cjs",
          },
        },
      }),
    );
    fx.write("app/node_modules/cond-pkg/dist/module.mjs", `export const which = "/module/";\n`);
    fx.write("app/node_modules/cond-pkg/dist/import.mjs", `export const which = "/import/";\n`);
    fx.write("app/node_modules/cond-pkg/dist/require.cjs", `module.exports = { which: "/require/" };\n`);
    fx.write(
      "app/vite.config.ts",
      `import { which } from "cond-pkg";
export default { base: which };
`,
    );
    const { json } = runExtract(fx);
    assert.equal(json.__ok, true, `extraction failed, got: ${JSON.stringify(json)}`);
    assert.equal(json.base, "/import/", 'Node picks the "import" target; esbuild\'s default picks "module"');
  } finally {
    fx.cleanup();
  }
});

testEsbuild("a data: URL import in the config loads", () => {
  const fx = monorepoFixture();
  try {
    const dataUrl =
      "data:text/javascript;base64," +
      Buffer.from(`export default "/from-data-url/";`).toString("base64");
    fx.write(
      "app/vite.config.ts",
      `import dataBase from ${JSON.stringify(dataUrl)};
export default { base: dataBase };
`,
    );
    const { json } = runExtract(fx);
    assert.equal(json.__ok, true, `extraction failed, got: ${JSON.stringify(json)}`);
    assert.equal(json.base, "/from-data-url/", "the data: module was bundled by esbuild, not file-URL-mangled");
  } finally {
    fx.cleanup();
  }
});

testEsbuild("__dirname and import.meta.url reflect each file's original location", () => {
  const fx = monorepoFixture();
  try {
    fx.write("pkg/src/dir.ts", `export const pkgDir: string = __dirname;\n`);
    fx.write(
      "app/vite.config.ts",
      `import { fileURLToPath } from "node:url";
import { pkgDir } from "../pkg/src/dir";
export default {
  define: {
    __CFG_SUB__: JSON.stringify(fileURLToPath(new URL("./sub", import.meta.url))),
    __PKG_DIR__: JSON.stringify(pkgDir),
  },
};
`,
    );
    const { json } = runExtract(fx);
    assert.equal(json.__ok, true, `extraction failed, got: ${JSON.stringify(json)}`);
    // The bundle runs from a throwaway run dir; both values must still name
    // the ORIGINAL files' locations, per inlined file (esbuild canonicalizes
    // paths, so compare against the realpath: /var vs /private/var on macOS).
    assert.equal(JSON.parse(json.define.__CFG_SUB__), path.join(fs.realpathSync(fx.appRoot), "sub"));
    assert.equal(JSON.parse(json.define.__PKG_DIR__), path.join(fs.realpathSync(fx.base), "pkg", "src"));
  } finally {
    fx.cleanup();
  }
});

testEsbuild("an uninstalled bare import stays bare-external and stderr names it", () => {
  const fx = monorepoFixture();
  try {
    // A dynamic import that never runs: the config must still LOAD (the bare
    // fallback exists so configs that resolve only at import time keep
    // working), and the bundler must say what it could not resolve and from
    // where -- the import-time error would name a deleted tmp bundle.
    fx.write(
      "app/vite.config.ts",
      `export const loadLater = () => import("oj-test-not-installed-pkg");
export default { base: "/still-loads/" };
`,
    );
    const { json, stderr } = runExtract(fx);
    assert.equal(json.__ok, true, `extraction failed, got: ${JSON.stringify(json)}`);
    assert.equal(json.base, "/still-loads/");
    assert.match(stderr, /could not resolve "oj-test-not-installed-pkg" imported from /);
    assert.ok(stderr.includes(fx.configPath), `warning must name the importer; stderr:\n${stderr}`);
  } finally {
    fx.cleanup();
  }
});
