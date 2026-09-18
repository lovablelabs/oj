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
import { execFileSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { asset, rpcSidecar, testWithEsbuild, tmpProject } from "./harness.mjs";

const testEsbuild = testWithEsbuild(test);

// base/
//   node_modules/esbuild        (symlinked from the start-app fixture)
//   app/vite.config.ts          imports ../pkg/src/plugin (inlined by esbuild)
//   pkg/src/plugin.ts           imports "only-dep" (bare)
//   pkg/node_modules/only-dep/  the ONLY place the dep exists
function monorepoFixture() {
  const fx = tmpProject({ prefix: "oj-cfg-ext-", linkEsbuild: true });
  fx.write("app/package.json", JSON.stringify({ name: "app", type: "module" }));
  fx.write(
    "app/vite.config.ts",
    `import { makePlugin, ojBase } from "../pkg/src/plugin";
export default {
  base: ojBase,
  plugins: [makePlugin()],
};
`,
  );
  fx.write("pkg/package.json", JSON.stringify({ name: "pkg", type: "module" }));
  fx.write(
    "pkg/src/plugin.ts",
    `import { fromDep } from "only-dep";
export const ojBase = fromDep;
export function makePlugin() {
  return {
    name: "pkg-plugin",
    config() {
      return { define: { __FROM_PKG__: JSON.stringify(fromDep) } };
    },
  };
}
`,
  );
  fx.write(
    "pkg/node_modules/only-dep/package.json",
    JSON.stringify({ name: "only-dep", version: "1.0.0", type: "module", main: "index.js" }),
  );
  fx.write("pkg/node_modules/only-dep/index.js", `export const fromDep = "/from-pkg-dep/";\n`);
  return {
    base: fx.root,
    appRoot: path.join(fx.root, "app"),
    configPath: path.join(fx.root, "app", "vite.config.ts"),
    write: fx.write,
    cleanup: fx.cleanup,
  };
}

// Run a copy of the extractor from a throwaway dir, the way it runs from a
// fresh cache dir: nothing from the fixture's nested node_modules is
// resolvable from there, and its tmp bundle lands there, not in the assets
// dir. Returns { json, stderr }.
function runExtract(fx) {
  const runDir = fs.mkdtempSync(path.join(os.tmpdir(), "oj-cfg-ext-run-"));
  try {
    const script = path.join(runDir, "vite-extract.mjs");
    fs.copyFileSync(asset("vite-extract.mjs"), script);
    // The exported extract(), as oj's in-process engine calls it; the wrapper
    // prints the result on stdout and the __stderr transcript on stderr.
    const wrapper = `
import { writeSync } from "node:fs";
import { pathToFileURL } from "node:url";
const [script, vite, root] = process.argv.slice(1);
const { extract } = await import(pathToFileURL(script).href);
const { __stderr = "", ...rest } = await extract({ vite, root, command: "serve", mode: "development", modeKind: "default" });
if (__stderr) writeSync(2, __stderr);
writeSync(1, JSON.stringify(rest));
process.exit(0);
`;
    const r = spawnSync(
      process.execPath,
      ["--input-type=module", "-e", wrapper, script, fx.configPath, fx.appRoot],
      { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"], timeout: 60_000 },
    );
    assert.equal(r.status, 0, `extractor exited ${r.status}; stderr:\n${r.stderr}`);
    let json;
    try {
      json = JSON.parse(r.stdout);
    } catch {
      assert.fail(`extractor wrote unparseable output: ${r.stdout}\nstderr:\n${r.stderr}`);
    }
    return { json, stderr: r.stderr };
  } finally {
    fs.rmSync(runDir, { recursive: true, force: true });
  }
}

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
  // Run a copy of the host from a throwaway dir (the cache-dir shape): its tmp
  // config bundle is written next to the running script, and that must never
  // be the checked-in assets dir, nor a dir whose parents hold node_modules.
  const runDir = fs.mkdtempSync(path.join(os.tmpdir(), "oj-cfg-ext-host-"));
  const hostScript = path.join(runDir, "plugin-host.mjs");
  fs.copyFileSync(asset("plugin-host.mjs"), hostScript);
  const host = rpcSidecar(hostScript, {
    args: [
      fx.configPath,
      JSON.stringify({
        pluginsFormat: "vite",
        config: { root: fx.appRoot },
        env: { command: "serve", mode: "development" },
        environment: { name: "client", mode: "dev" },
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.appRoot },
    cwd: fx.appRoot,
  });
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
    fs.rmSync(runDir, { recursive: true, force: true });
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
