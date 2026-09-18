// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// The deps pre-seed (optimize-env.mjs): a one-shot child runs Vite's own dep
// optimizer for the server environments so the plugin host — which builds the
// app's real DevEnvironments in-process — finds a warm cache and never runs a
// rolldown build inside oj's process. The acceptance here is HASH acceptance,
// not file existence: after the seed, Vite's own DevEnvironment optimizer
// (resolved exactly the way the host resolves it) must load the cache and not
// re-optimize.

import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..", "..");
const fixture = path.join(repo, "e2e", "fixtures", "start-app");
const script = path.join(repo, "crates", "oj_server", "src", "assets", "optimize-env.mjs");

const viteDist = path.join(fixture, "node_modules", "vite", "package.json");
const installed = fs.existsSync(viteDist) && fs.existsSync(path.join(fixture, "node_modules", "react"));

function makeApp() {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-optimize-env-"));
  const app = path.join(tmp, "app");
  fs.mkdirSync(app);
  fs.writeFileSync(
    path.join(app, "package.json"),
    JSON.stringify({ name: "oj-preseed-fixture", private: true, type: "module" }),
  );
  fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");
  // Its own cacheDir: the default would resolve through the node_modules
  // symlink into the shared fixture and collide across runs.
  fs.writeFileSync(
    path.join(app, "vite.config.mjs"),
    [
      "export default {",
      `  cacheDir: ${JSON.stringify(path.join(app, ".vite-cache"))},`,
      "  environments: {",
      '    ssr: { optimizeDeps: { include: ["react"] } },',
      "  },",
      "};",
      "",
    ].join("\n"),
  );
  return { tmp, app };
}

function runSeed(app) {
  execFileSync(process.execPath, [script], {
    cwd: app,
    stdio: ["ignore", "inherit", "inherit"],
    env: {
      ...process.env,
      OJ_APP_ROOT: app,
      OJ_ENV_MODE: "dev",
      OJ_PRESEED_REPORT: path.join(app, "optimize-env-report.json"),
      NO_COLOR: "1",
    },
  });
}

test("the pre-seed writes a deps cache Vite's own optimizer accepts as warm", { skip: !installed && "fixture deps not installed" }, async () => {
  const { tmp, app } = makeApp();
  try {
    runSeed(app);
    const metaPath = path.join(app, ".vite-cache", "deps_ssr", "_metadata.json");
    assert.ok(fs.existsSync(metaPath), "deps_ssr/_metadata.json committed");
    const meta = JSON.parse(fs.readFileSync(metaPath, "utf8"));
    assert.ok(meta.optimized.react, "react was pre-bundled for the ssr environment");
    assert.ok(meta.lockfileHash, "metadata carries Vite's lockfileHash");
    assert.ok(meta.configHash, "metadata carries Vite's configHash");
    const before = fs.statSync(metaPath).mtimeMs;

    // Report for the parent's stamp: the seeded env and where the metadata is.
    const report = JSON.parse(fs.readFileSync(path.join(app, "optimize-env-report.json"), "utf8"));
    assert.equal(report.failed, false);
    assert.deepEqual(report.seeded.map((s) => s.name), ["ssr"]);
    assert.equal(report.seeded[0].metadataPath, metaPath);

    // The acceptance: resolve the config exactly the way the plugin host's
    // buildEnvironments does, run the in-host environment's own optimizer
    // init, and require it to LOAD the cache — a re-optimization would
    // replace the metadata file (fresh mtime and a re-created dir).
    const vite = await import(
      pathToFileURL(path.join(fixture, "node_modules", "vite", "dist", "node", "index.js")).href
    );
    const rc = await vite.resolveConfig(
      { root: app, configFile: undefined, mode: "dev" },
      "serve",
      "development",
      "development",
    );
    const de = new vite.DevEnvironment("ssr", rc, { hot: false });
    assert.ok(de.depsOptimizer, "the ssr environment has an optimizer to satisfy");
    await de.init();
    await de.depsOptimizer.init();
    assert.ok(de.depsOptimizer.metadata.optimized.react, "optimizer served react from the cache");
    assert.equal(
      fs.statSync(metaPath).mtimeMs,
      before,
      "the in-host optimizer loaded the seeded cache instead of re-optimizing",
    );
    await de.close();

    // Second seed run against the warm cache: also a load, never a rebuild.
    runSeed(app);
    assert.equal(fs.statSync(metaPath).mtimeMs, before, "a warm re-seed is a no-op");
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});

test("environments without an enabled optimizer seed nothing and report cleanly", { skip: !installed && "fixture deps not installed" }, async () => {
  const { tmp, app } = makeApp();
  try {
    // No include list: Vite disables the ssr optimizer (noDiscovery default
    // for server consumers), so there is nothing to pre-seed.
    fs.writeFileSync(
      path.join(app, "vite.config.mjs"),
      `export default { cacheDir: ${JSON.stringify(path.join(app, ".vite-cache"))} };\n`,
    );
    runSeed(app);
    assert.ok(!fs.existsSync(path.join(app, ".vite-cache", "deps_ssr")), "no ssr cache invented");
    const report = JSON.parse(fs.readFileSync(path.join(app, "optimize-env-report.json"), "utf8"));
    assert.equal(report.failed, false);
    assert.deepEqual(report.seeded, []);
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});
