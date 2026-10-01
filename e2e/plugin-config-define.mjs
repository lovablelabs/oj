// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// A plugin's config() hook returning `define` must reach the compile: Vite merges
// the hook's result into the resolved config (mergeConfig, plugin value wins), so
// `__FROM_PLUGIN__` is replaced in dev and in the build. oj's plugin host ran the
// hook but kept the result to itself; the Rust side never saw the define.

import { spawn, execSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const PORT = 6403;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-cfgdefine-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "cfgdefine", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "oj.config.ts"),
  `export default { define: { __FROM_USER__: JSON.stringify("user-define"), __SHARED__: JSON.stringify("user-shared") } };\n`,
);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `window.__P = __FROM_PLUGIN__;\nwindow.__U = __FROM_USER__;\nwindow.__S = __SHARED__;\nwindow.__N = __PLUGIN_NUM__ + 1;\nwindow.__E = import.meta.env.VITE_FROM_HOOK;\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `export default [{
    name: "definer",
    config() {
      process.env.VITE_FROM_HOOK = "hook-env";
      return { define: { __FROM_PLUGIN__: JSON.stringify("plugin-define"), __SHARED__: JSON.stringify("plugin-shared"), __PLUGIN_NUM__: 41 } };
    },
  }];\n`,
);

// A prefixed var a config() hook sets reaches import.meta.env in dev and in the
// build, as in Vite (env is loaded after the config hooks run).
function checkHookEnv(code, label) {
  assert.match(code, /["'`]hook-env["'`]/, `${label}: config() hook env reaches import.meta.env:\n${code}`);
}

// The minifier may re-quote string literals; accept any quote style.
function check(code, label) {
  assert.match(code, /["'`]plugin-define["'`]/, `${label}: plugin config() define replaced:\n${code}`);
  assert.match(code, /["'`]user-define["'`]/, `${label}: user define still replaced:\n${code}`);
  assert.match(
    code,
    /["'`]plugin-shared["'`]/,
    `${label}: plugin value wins over the user's for the same key (Vite mergeConfig):\n${code}`,
  );
  assert.match(code, /41\s*\+\s*1|42/, `${label}: non-string define values are inlined as JSON:\n${code}`);
  assert.doesNotMatch(code, /__FROM_PLUGIN__|__PLUGIN_NUM__/, `${label}: no define identifier left behind:\n${code}`);
}

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${PORT}/`);
  const devCode = await (await fetch(`http://localhost:${PORT}/src/main.js`)).text();
  check(devCode, "dev");
  checkHookEnv(devCode, "dev");
  srv.kill("SIGKILL");
  await sleep(300);

  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  const r = spawnSync(oj, ["build", app], { encoding: "utf8" });
  assert.equal(r.status, 0, `build failed:\n${r.stderr}`);
  const js = fs
    .readdirSync(path.join(app, "dist", "assets"))
    .filter((f) => f.endsWith(".js"))
    .map((f) => fs.readFileSync(path.join(app, "dist", "assets", f), "utf8"))
    .join("\n");
  check(js, "build");
  checkHookEnv(js, "build");

  // Slow boot: the host's top-level init (a 5s configureServer, standing in for
  // plugin fleets / Miniflare) outlives a shrunk per-RPC timeout. RPC sends are
  // init-gated, so boot blocks briefly-but-correctly instead of the boot RPCs
  // (config defines included) burning their own timeouts and permanently
  // snapshotting wrong defaults — the config() define must still reach a served
  // module, byte-identical to the fast boot.
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  fs.writeFileSync(
    path.join(app, "oj.plugins.mjs"),
    `export default [{
      name: "definer",
      config() {
        return { define: { __FROM_PLUGIN__: JSON.stringify("plugin-define"), __SHARED__: JSON.stringify("plugin-shared"), __PLUGIN_NUM__: 41 } };
      },
      async configureServer() { await new Promise((r) => setTimeout(r, 5000)); },
    }];\n`,
  );
  const SLOW_PORT = PORT + 1;
  const slow = spawn(oj, ["dev", app, "--port", String(SLOW_PORT)], {
    stdio: "ignore",
    env: { ...process.env, OJ_PLUGIN_TIMEOUT: "2" },
  });
  try {
    const up = await waitUp(`http://localhost:${SLOW_PORT}/`).then(
      () => true,
      () => false,
    );
    assert.ok(up, "slow-boot dev server never came up (boot RPCs raced init instead of waiting)");
    check(await (await fetch(`http://localhost:${SLOW_PORT}/src/main.js`)).text(), "slow-boot dev");
  } finally {
    slow.kill("SIGKILL");
    await sleep(300);
  }
  console.log("PLUGIN-CONFIG-DEFINE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PLUGIN-CONFIG-DEFINE E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
