// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// When the app has Vite installed, plugins' config/configResolved/configureServer
// see the app's own resolved Vite config (build.outDir/sourcemap/rollupOptions,
// cacheDir, env, envPrefix, assetsInclude(), css, ssr, worker) with oj's plugin
// instances spliced in, not a synthesized config whose build.outDir is always
// "dist". A fake `vite` package stands in for the real one.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const PORT = 5504;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oj-rescfg-")));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.mkdirSync(path.join(app, "node_modules", "vite"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "rescfg", version: "1.0.0", type: "module" }));
fs.writeFileSync(path.join(app, "node_modules", "vite", "package.json"), JSON.stringify({ name: "vite", version: "8.0.0-fake", type: "module", main: "index.js", exports: { ".": "./index.js" } }));
fs.writeFileSync(
  path.join(app, "node_modules", "vite", "index.js"),
  `export async function resolveConfig(inline, command, mode) {
  const root = inline.root;
  return {
    root, base: "/", mode, command, isProduction: mode === "production",
    build: { outDir: "vite-out", sourcemap: true, assetsDir: "static", rollupOptions: { output: { manualChunks: {} } } },
    cacheDir: root + "/node_modules/.vite-fake",
    env: { VITE_FROM_VITE: "yes", MODE: mode, DEV: command === "serve", PROD: command !== "serve", SSR: false, BASE_URL: "/" },
    envPrefix: "VITE_",
    assetsInclude: (f) => String(f).endsWith(".xyz"),
    css: { modules: { localsConvention: "camelCase" }, preprocessorOptions: {} },
    ssr: { noExternal: ["some-lib"], external: [] },
    worker: { format: "es" },
    resolve: { alias: [], extensions: [".mjs", ".js", ".ts"] },
    server: { port: 1, headers: {} },
    plugins: [{ name: "fresh-instance-from-vite" }],
    environments: { client: {}, ssr: {} },
    logger: { info() {}, warn() {}, warnOnce() {}, error() {}, clearScreen() {}, hasErrorLogged: () => false, hasWarned: false },
    createResolver: () => async () => undefined,
  };
}
`,
);
const marks = fs.mkdtempSync(path.join(os.tmpdir(), "oj-rescfg-marks-"));
fs.writeFileSync(path.join(app, "src", "main.js"), `window.__ok = 1;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "vite.config.js"),
  `import fs from "node:fs";
export default {
  plugins: [{
    name: "inspector",
    config() { return { define: { __FROM_OJ_INSTANCE__: "1" } }; },
    configResolved(rc) {
      fs.writeFileSync(${JSON.stringify(path.join(marks, "resolved.json"))}, JSON.stringify({
        outDir: rc.build.outDir, sourcemap: rc.build.sourcemap, assetsDir: rc.build.assetsDir,
        manualChunks: typeof rc.build.rollupOptions?.output?.manualChunks,
        cacheDir: rc.cacheDir, envFromVite: rc.env?.VITE_FROM_VITE, envPrefix: rc.envPrefix,
        assetsInclude: typeof rc.assetsInclude === "function" ? rc.assetsInclude("a.xyz") : null,
        cssLocals: rc.css?.modules?.localsConvention, noExternal: rc.ssr?.noExternal, worker: rc.worker?.format,
        pluginNames: rc.plugins.map((p) => p.name), ownDefine: rc.define.__FROM_OJ_INSTANCE__, root: rc.root,
      }));
    },
  }],
};
`,
);

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${PORT}/`);
  await settles(() => fs.existsSync(path.join(marks, "resolved.json")));
  const rc = JSON.parse(fs.readFileSync(path.join(marks, "resolved.json"), "utf8"));
  assert.equal(rc.outDir, "vite-out", "build.outDir comes from the app's resolved Vite config, not the synthesized 'dist'");
  assert.equal(rc.sourcemap, true);
  assert.equal(rc.assetsDir, "static");
  assert.equal(rc.manualChunks, "object", "rollupOptions kept");
  assert.equal(rc.cacheDir, app + "/node_modules/.vite-fake");
  assert.equal(rc.envFromVite, "yes", "resolved env object present");
  assert.equal(rc.envPrefix, "VITE_");
  assert.equal(rc.assetsInclude, true, "assetsInclude is a real function");
  assert.equal(rc.cssLocals, "camelCase");
  assert.deepEqual(rc.noExternal, ["some-lib"]);
  assert.equal(rc.worker, "es");
  assert.equal(rc.root, app);
  assert.ok(rc.pluginNames.includes("inspector") && !rc.pluginNames.includes("fresh-instance-from-vite"), `oj's plugin instances are spliced in, Vite's fresh ones dropped: ${rc.pluginNames}`);
  assert.equal(rc.ownDefine, "1", "oj's own instances' config hooks still apply on top");
  console.log("PLUGIN-RESOLVED-CONFIG E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PLUGIN-RESOLVED-CONFIG E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
  fs.rmSync(marks, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
