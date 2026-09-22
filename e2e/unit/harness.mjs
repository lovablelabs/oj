// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Shared test harness for driving oj's Node sidecars from `node --test`.
// Two shapes, matching how the Rust side spawns them:
//   - runSidecar:  one-shot `node <sidecar> <jsonArg>` -> parsed stdout JSON
//                  (optimize-deps.mjs)
//   - rpcSidecar:  long-lived newline-delimited JSON RPC over stdin/stdout
//                  (plugin-host.mjs)

import assert from "node:assert/strict";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import readline from "node:readline";

const here = path.dirname(fileURLToPath(import.meta.url));
export const repo = path.join(here, "..", "..");
export const asset = (rel) => path.join(repo, "crates/oj_server/src/assets", rel);

const esbuildSrc = path.join(repo, "e2e/fixtures/start-app/node_modules/esbuild");
export const hasEsbuildFixture = () => fs.existsSync(esbuildSrc);

// Wrap `node:test`'s `test` so cases that need the pre-bundler's esbuild skip
// (rather than hard-fail) when the start-app fixture has no node_modules, the
// same convention optimize-deps.test.mjs uses.
export function testWithEsbuild(test) {
  return hasEsbuildFixture()
    ? test
    : (name, fn) => test(name, { skip: "fixture esbuild not installed" }, () => {});
}

// Same convention for rolldown (vite 8's bundler, hoisted by the start-app
// fixture install): the config-bundler fallback tests need the real thing.
const rolldownSrc = path.join(repo, "e2e/fixtures/start-app/node_modules/rolldown");
export const hasRolldownFixture = () => fs.existsSync(rolldownSrc);
export function testWithRolldown(test) {
  return hasRolldownFixture()
    ? test
    : (name, fn) => test(name, { skip: "fixture rolldown not installed" }, () => {});
}
// Symlink the fixture's rolldown into a package root's node_modules (Node
// resolves its native bindings from the symlink's realpath, next to the real
// package). The root may also be a dependency dir (the nested,
// rolldown-under-vite shape), so create its node_modules if needed.
export function linkRolldown(pkgRoot) {
  fs.mkdirSync(path.join(pkgRoot, "node_modules"), { recursive: true });
  fs.symlinkSync(rolldownSrc, path.join(pkgRoot, "node_modules", "rolldown"));
}

// A throwaway project dir with a node_modules/ and package.json. `linkEsbuild`
// symlinks the fixture's real esbuild (+ @esbuild binary) in, so the optimizer
// resolves it without a per-test install.
export function tmpProject({ prefix = "oj-fx-", pkgJson = { name: "fx" }, linkEsbuild = false } = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), prefix));
  fs.mkdirSync(path.join(root, "node_modules"), { recursive: true });
  fs.writeFileSync(path.join(root, "package.json"), JSON.stringify(pkgJson));
  if (linkEsbuild) {
    fs.symlinkSync(esbuildSrc, path.join(root, "node_modules", "esbuild"));
    const scoped = path.join(repo, "e2e/fixtures/start-app/node_modules/@esbuild");
    if (fs.existsSync(scoped)) fs.symlinkSync(scoped, path.join(root, "node_modules", "@esbuild"));
  }
  return {
    root,
    // Write a fake dependency under node_modules/<name>/.
    pkg(name, main, files) {
      const dir = path.join(root, "node_modules", name);
      fs.mkdirSync(dir, { recursive: true });
      fs.writeFileSync(path.join(dir, "package.json"), JSON.stringify({ name, version: "1.0.0", main }));
      for (const [f, c] of Object.entries(files)) fs.writeFileSync(path.join(dir, f), c);
    },
    // Write a file relative to the project root, creating parent dirs.
    write(rel, content) {
      const p = path.join(root, rel);
      fs.mkdirSync(path.dirname(p), { recursive: true });
      fs.writeFileSync(p, content);
    },
    cleanup() {
      fs.rmSync(root, { recursive: true, force: true });
    },
  };
}

// Shape A: run a one-shot module's exported entry (`optimize(config)` for
// optimize-deps.mjs) the way oj's in-process JS engine calls it — in a fresh
// node child per run for isolation — and return the parsed result. Throws on
// a non-zero exit; the thrown error carries `.stdout`/`.stderr`/`.status` for
// error-path assertions.
const ONE_SHOT_WRAPPER = `
import { writeSync } from "node:fs";
import { pathToFileURL } from "node:url";
const [script, exportName, cfg] = process.argv.slice(1);
const mod = await import(pathToFileURL(script).href);
const out = await mod[exportName](JSON.parse(cfg));
writeSync(1, JSON.stringify(out));
process.exit(0);
`;

export function runSidecar(sidecarRel, config, { cwd, timeout = 30_000, exportName = "optimize" } = {}) {
  const out = execFileSync(
    "node",
    ["--input-type=module", "-e", ONE_SHOT_WRAPPER, asset(sidecarRel), exportName, JSON.stringify(config)],
    {
      cwd,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      timeout,
      maxBuffer: 64 * 1024 * 1024,
    },
  );
  return JSON.parse(out);
}

// Shape B: spawn a long-lived sidecar and speak newline-delimited JSON. `send`
// writes one frame and resolves with the next stdout frame (parsed); a frame
// that isn't the reply you expect is returned as-is, so callers that trigger
// host->driver requests can dispatch on it. Always `close()` in a finally.
export function rpcSidecar(sidecarRel, { args = [], env, cwd, controlToken } = {}) {
  // An absolute path runs a copy of the sidecar from elsewhere (a test that
  // reproduces the cache-dir shape); a relative name runs the asset in place.
  const script = path.isAbsolute(sidecarRel) ? sidecarRel : asset(sidecarRel);
  const child = spawn("node", [script, ...args], {
    cwd,
    env: {
      ...process.env,
      // The plugin host frames every protocol line with this token when set
      // (the Rust spawn always sets one); the reader below then mimics the
      // Rust side: unframed lines are plugin prints, never protocol.
      ...(controlToken ? { OJ_CONTROL_TOKEN: controlToken } : {}),
      ...(env ?? {}),
    },
    stdio: ["pipe", "pipe", "pipe"],
  });
  const frames = [];
  let waiter = null;
  // The plugin host pushes `{ ojServeInfo }` once its top-level init completes
  // (the Rust reader consumes it out-of-band the same way); it is not a reply
  // to anything, so it never enters the frame queue. Tests that care read it
  // via `serveInfo()` / `serveInfoPushed()`. Re-pushed until acked, so a
  // count is kept too (`serveInfoPushCount()`); `ackServeInfo()` sends the
  // { ojServeInfoAck } the Rust side sends, stopping the re-push.
  let serveInfoPushed;
  let serveInfoPushCount = 0;
  let serveInfoResolve;
  const serveInfoArrived = new Promise((r) => (serveInfoResolve = r));
  // The host's unconditional init-complete signal ({ ojInit: true }, sent in
  // BOTH modes; build mode has no ojServeInfo push). Out-of-band like the
  // serve info: it must never be consumed as an RPC reply.
  let initPushed = false;
  let initResolve;
  const initArrived = new Promise((r) => (initResolve = r));
  // The host's init milestones ({ ojInitProgress }), in arrival order.
  const initStages = [];
  readline.createInterface({ input: child.stdout }).on("line", (line) => {
    if (!line.trim()) return;
    if (controlToken) {
      if (!line.startsWith(controlToken)) return;
      line = line.slice(controlToken.length);
    }
    try {
      const parsed = JSON.parse(line);
      if (parsed && typeof parsed === "object" && "ojServeInfo" in parsed) {
        serveInfoPushed = parsed.ojServeInfo;
        serveInfoPushCount += 1;
        serveInfoResolve(parsed.ojServeInfo);
        return;
      }
      if (parsed && typeof parsed === "object" && "ojInit" in parsed) {
        initPushed = true;
        initResolve();
        return;
      }
      // Init milestones ({ ojInitProgress }) are out-of-band control pushes
      // like the two above (the Rust reader consumes them for its stall
      // monitor): they must never be handed to a send() as its reply.
      if (parsed && typeof parsed === "object" && "ojInitProgress" in parsed) {
        initStages.push(parsed.ojInitProgress);
        return;
      }
    } catch {}
    if (waiter) {
      const w = waiter;
      waiter = null;
      w(line);
    } else {
      frames.push(line);
    }
  });
  let stderr = "";
  child.stderr.on("data", (d) => {
    stderr += d.toString();
  });
  // If the sidecar dies mid-test, a write to its stdin would emit EPIPE; absorb
  // it so the pending send's timeout reports the failure instead of crashing.
  child.stdin.on("error", () => {});

  const nextLine = (ms) =>
    new Promise((res, rej) => {
      if (frames.length) return res(frames.shift());
      const to = setTimeout(() => rej(new Error(`sidecar rpc timeout after ${ms}ms; stderr:\n${stderr}`)), ms);
      waiter = (line) => {
        clearTimeout(to);
        res(line);
      };
    });

  return {
    child,
    stderr: () => stderr,
    // The host's serve-info push: awaits it (`serveInfo()`), or peeks at what
    // has arrived so far (`serveInfoPushed()`, undefined until the push lands).
    serveInfo: () => serveInfoArrived,
    serveInfoPushed: () => serveInfoPushed,
    serveInfoPushCount: () => serveInfoPushCount,
    // The init-complete signal: awaits it (`initSignal()`), or peeks
    // (`initPushed()`, false until it lands).
    initSignal: () => initArrived,
    initPushed: () => initPushed,
    initStages: () => [...initStages],
    ackServeInfo() {
      child.stdin.write('{"ojServeInfoAck":true}\n');
    },
    async nextFrame(ms = 10_000) {
      return JSON.parse(await nextLine(ms));
    },
    async send(msg, ms = 10_000) {
      child.stdin.write(JSON.stringify(msg) + "\n");
      return JSON.parse(await nextLine(ms));
    },
    close() {
      try {
        child.stdin.end();
        child.kill("SIGKILL");
      } catch {
        // already gone
      }
    },
  };
}

// The monorepo config fixture the config-bundling suites share (issue #146's
// shape): the config relatively imports a sibling workspace package's TS
// source (inlined by the bundler), whose bare dep lives only under the
// sibling's own node_modules (externalized by resolved path). `bundler` links
// the start-app fixture's real esbuild or rolldown into the root.
export function configMonorepoFixture({ prefix, bundler }) {
  const fx = tmpProject({ prefix, linkEsbuild: bundler === "esbuild" });
  if (bundler === "rolldown") linkRolldown(fx.root);
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
// dir. Returns { json, stderr }. extract() reports a config that failed to
// evaluate as { __ok: false } rather than throwing, so the wrapper exits 0
// either way.
const EXTRACT_WRAPPER = `
import { writeSync } from "node:fs";
import { pathToFileURL } from "node:url";
const [script, vite, root] = process.argv.slice(1);
const { extract } = await import(pathToFileURL(script).href);
const { __stderr = "", ...rest } = await extract({ vite, root, command: "serve", mode: "development", modeKind: "default" });
if (__stderr) writeSync(2, __stderr);
writeSync(1, JSON.stringify(rest));
process.exit(0);
`;
export function runExtract(fx, { prefix = "oj-extract-run-" } = {}) {
  const runDir = fs.mkdtempSync(path.join(os.tmpdir(), prefix));
  try {
    const script = path.join(runDir, "vite-extract.mjs");
    fs.copyFileSync(asset("vite-extract.mjs"), script);
    const r = spawnSync(
      process.execPath,
      ["--input-type=module", "-e", EXTRACT_WRAPPER, script, fx.configPath, fx.appRoot],
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

// Boot a copy of the plugin host from a throwaway dir (the cache-dir shape:
// its tmp config bundle lands next to the running script, and that must never
// be the checked-in assets dir) on `fx.configPath` as a vite-format config.
// The returned cleanup removes the run dir; callers still close() the host.
export function bootHost(fx, { prefix = "oj-host-run-" } = {}) {
  const runDir = fs.mkdtempSync(path.join(os.tmpdir(), prefix));
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
  return { host, cleanup: () => fs.rmSync(runDir, { recursive: true, force: true }) };
}
