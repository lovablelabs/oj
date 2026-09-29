// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// The plugin host pushes `{ ojServeInfo: { middlewarePort, runnerEnvironments } }`
// on stdout the moment its top-level init completes. The RPC listener only
// registers after every top-level await, so on a slow boot (many plugins, a
// Miniflare boot inside configureServer) the Rust side's getServeInfo RPC times
// out — the push is what lets it activate the middleware path late instead of
// silently serving every document from the Node SSR runner. These tests pin:
// the push exists, it carries the middleware port, and it is emitted BEFORE any
// RPC is answered (so an answered RPC implies the push already arrived — the
// ordering Rust's serve_info fast path relies on).

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

test("a delayed configureServer still yields the ojServeInfo push, ahead of any RPC reply", async () => {
  const fx = tmpProject({ prefix: "oj-serveinfo-push-" });
  // configureServer stalls like a heavy boot (plugin fleets, Miniflare): the
  // host must not answer RPCs until it finishes, and the push must come first.
  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "slow-middleware",
       async configureServer(server) {
         await new Promise((r) => setTimeout(r, 600));
         server.middlewares.use((req, res, next) => next());
       },
     }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    // Sent while the host is still mid-init, like Rust's boot-time RPCs.
    const reply = await host.send({ id: 7, hook: "getServeInfo" }, 20_000);
    assert.equal(reply.id, 7);

    // The push landed before the RPC was answered (the harness consumes it
    // out-of-band, so an answered RPC with no recorded push means it never
    // came, or came late).
    const pushed = host.serveInfoPushed();
    assert.ok(pushed, "the ojServeInfo push must precede the first RPC reply");
    assert.equal(typeof pushed.middlewarePort, "number", "the push carries the middleware port");
    assert.equal(pushed.runnerEnvironments, false);
    assert.equal(JSON.parse(reply.result).middlewarePort, pushed.middlewarePort);
  } finally {
    host.close();
    fx.cleanup();
  }
});

// With OJ_CONTROL_TOKEN in the spawn env (the Rust spawn always sets one) the
// host frames EVERY protocol line — pushes and RPC replies — with the token, so
// a plugin's print sharing stdout can never be parsed as protocol: a forged,
// unframed ojServeInfo line is ignored, and the framed real push wins. The push
// is also re-sent until the driver ACKs (a spliced copy must not be lost
// forever) and stops on { ojServeInfoAck }.
test("control-token framing: forged unframed pushes are ignored, re-push stops on ack", async () => {
  const fx = tmpProject({ prefix: "oj-serveinfo-token-" });
  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "forger",
       async configureServer(server) {
         // A plugin (or attacker-controlled content it echoes) printing the
         // exact JSON shape, unframed: must never be accepted as protocol.
         process.stdout.write(JSON.stringify({ ojServeInfo: { middlewarePort: 9999, runnerEnvironments: true } }) + "\\n");
         // An unterminated partial line: the framed push must survive it.
         process.stdout.write("partial log without newline ");
         server.middlewares.use((req, res, next) => next());
       },
     }];\n`,
  );
  const token = "oj-test-token-1:";
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
    controlToken: token,
  });
  try {
    const pushed = await host.serveInfo();
    assert.equal(typeof pushed.middlewarePort, "number");
    assert.notEqual(pushed.middlewarePort, 9999, "the forged unframed push must be ignored");
    assert.equal(pushed.runnerEnvironments, false);

    // Framed RPC replies still round-trip (past the dangling partial line).
    const reply = await host.send({ id: 3, hook: "getServeInfo" }, 20_000);
    assert.equal(reply.id, 3);
    assert.equal(JSON.parse(reply.result).middlewarePort, pushed.middlewarePort);

    // Un-acked, the push re-sends about once a second...
    await new Promise((r) => setTimeout(r, 2600));
    const beforeAck = host.serveInfoPushCount();
    assert.ok(beforeAck >= 2, `expected re-pushes without an ack, saw ${beforeAck}`);
    // ...and the ack stops it.
    host.ackServeInfo();
    await new Promise((r) => setTimeout(r, 400));
    const settled = host.serveInfoPushCount();
    await new Promise((r) => setTimeout(r, 2400));
    assert.equal(host.serveInfoPushCount(), settled, "pushes must stop after the ack");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("with no middleware registered the push still arrives, with a null port", async () => {
  const fx = tmpProject({ prefix: "oj-serveinfo-none-" });
  fx.write("oj.plugins.mjs", `export default [{ name: "no-middleware", transform: (code) => null }];\n`);
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const pushed = await host.serveInfo();
    assert.equal(pushed.middlewarePort, null);
    assert.equal(pushed.runnerEnvironments, false);
  } finally {
    host.close();
    fx.cleanup();
  }
});

// The unconditional init-complete signal ({ ojInit: true }): sent in BOTH
// modes once the RPC listener is registered. Build mode has no ojServeInfo
// push at all, so without it Rust's init gate only released on the first
// reply — a hanging first hook waited out the whole init deadline blamed on
// initialization instead of failing on the per-call timeout.
test("build mode sends the ojInit signal (no serve-info push exists there)", async () => {
  const fx = tmpProject({ prefix: "oj-init-build-" });
  fx.write("oj.plugins.mjs", `export default [{ name: "build-only", transform: (code) => null }];\n`);
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root, env: { command: "build" } })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    await host.initSignal();
    assert.equal(host.serveInfoPushed(), undefined, "build mode must not push serve info");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("serve mode sends ojInit too, before any RPC reply", async () => {
  const fx = tmpProject({ prefix: "oj-init-serve-" });
  fx.write("oj.plugins.mjs", `export default [{ name: "plain", transform: (code) => null }];\n`);
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const reply = await host.send({ id: 1, hook: "getPluginCount" }, 20_000);
    assert.equal(reply.id, 1);
    assert.ok(host.initPushed(), "ojInit must precede the first RPC reply");
  } finally {
    host.close();
    fx.cleanup();
  }
});

// The resync ack is 202-style: /__oj_invalidate {"resync":true} answers 204 on
// ENQUEUE, and a separate control-plane { ojResyncDone } push tells the Rust
// side when the resync actually EXECUTED (the invalidate queue drained to it).
// Duplicates coalesce into the pending resync — retries never stack
// full-reloads — and the done push still arrives.
test("a resync is ACKed on enqueue and signals ojResyncDone when it executes", async () => {
  const fx = tmpProject({ prefix: "oj-resync-done-" });
  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "with-middleware",
       configureServer(server) {
         server.middlewares.use((req, res, next) => next());
       },
     }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const pushed = await host.serveInfo();
    assert.equal(typeof pushed.middlewarePort, "number");
    const post = () =>
      fetch(`http://127.0.0.1:${pushed.middlewarePort}/__oj_invalidate`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: '{"resync":true}',
      });
    // Two enqueues back to back: the second coalesces into the pending one.
    const [a, b] = await Promise.all([post(), post()]);
    assert.equal(a.status, 204, "the ack answers on enqueue");
    assert.equal(b.status, 204, "a coalesced duplicate is ACKed too");
    const frame = await host.nextFrame(10_000);
    assert.deepEqual(frame, { ojResyncDone: true }, "the completion push follows the execution");
  } finally {
    host.close();
    fx.cleanup();
  }
});
