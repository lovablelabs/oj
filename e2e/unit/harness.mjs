// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Shared test harness for driving oj's Node sidecars from `node --test`.
// Two shapes, matching how the Rust side spawns them:
//   - runSidecar:  one-shot `node <sidecar> <jsonArg>` -> parsed stdout JSON
//                  (optimize-deps.mjs)
//   - rpcSidecar:  long-lived newline-delimited JSON RPC over stdin/stdout
//                  (plugin-host.mjs)

import { execFileSync, spawn } from "node:child_process";
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
