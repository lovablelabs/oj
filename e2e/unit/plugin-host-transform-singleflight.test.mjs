// SPDX-License-Identifier: MIT
//
// Transform-chain singleflight: concurrent identical transform requests share
// one chain run instead of re-running every plugin hook per waiter. Only
// CONCURRENT calls join (the in-flight entry is removed on settle): a later
// identical call executes again, differing inputs execute separately, errors
// reject every waiter, and a retry after an error executes again.

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

const N_CONCURRENT = 8;
const HOOK_SLEEP_MS = 150;

function pluginFile(fx, mid, mid2, throwId) {
  fx.write(
    "oj.plugins.mjs",
    [
      `const MID = ${JSON.stringify(mid)};`,
      `const MID2 = ${JSON.stringify(mid2)};`,
      `const THROW = ${JSON.stringify(throwId)};`,
      "let calls = 0;",
      "const sleep = (ms) => new Promise((r) => setTimeout(r, ms));",
      "export default [{",
      "  name: 'singleflight-probe',",
      "  async transform(code, id) {",
      "    if (id !== MID && id !== MID2 && id !== THROW) return null;",
      "    calls += 1;",
      "    const n = calls;",
      "    await sleep(HOOK_MS);".replace("HOOK_MS", String(HOOK_SLEEP_MS)),
      "    if (id === THROW && n === 1) throw new Error('singleflight-boom');",
      "    return { code: `// run ${n}\\n` + code };",
      "  },",
      "}];",
      "",
    ].join("\n"),
  );
}

function spawnHost(fx) {
  return rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        config: { root: fx.root },
        env: { command: "serve", mode: "development" },
        environment: { name: "client", mode: "dev" },
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
}

// Fire N requests without waiting between them, then collect one reply frame
// per id. The host dispatches stdin lines concurrently, so slow hooks overlap.
async function sendConcurrent(host, msgs, ms = 15000) {
  const ids = new Set(msgs.map((m) => m.id));
  for (const m of msgs) host.child.stdin.write(JSON.stringify(m) + "\n");
  const out = new Map();
  const t0 = Date.now();
  while (out.size < msgs.length) {
    assert.ok(Date.now() - t0 < ms, `timed out waiting for ${msgs.length} replies (got ${out.size})`);
    const frame = await host.nextFrame(5000);
    if (frame == null || typeof frame !== "object") continue;
    if (frame.rpc != null) {
      host.child.stdin.write(JSON.stringify({ rpcReply: frame.rpc, result: null }) + "\n");
      continue;
    }
    if (!ids.has(frame.id)) continue;
    out.set(frame.id, frame);
  }
  return msgs.map((m) => out.get(m.id));
}

const runOf = (result) => {
  const out = JSON.parse(result);
  const m = /^\/\/ run (\d+)\n/.exec(out.code);
  assert.ok(m, `expected a run marker in transformed code, got: ${out.code.slice(0, 80)}`);
  return { n: Number(m[1]), out };
};

test("concurrent identical transforms run the hook once and all resolve deep-equal; a later call runs again", async () => {
  const fx = tmpProject({ prefix: "oj-sf-" });
  const mid = path.join(fx.root, "a.js");
  const mid2 = path.join(fx.root, "b.js");
  const throwId = path.join(fx.root, "throw.js");
  pluginFile(fx, mid, mid2, throwId);
  const host = spawnHost(fx);
  try {
    const code = "export const a = 1;\n";
    const frames = await sendConcurrent(
      host,
      Array.from({ length: N_CONCURRENT }, (_, i) => ({ id: 101 + i, hook: "transform", args: [code, mid, "null"] })),
    );
    for (const f of frames) assert.equal(f.error, undefined, `concurrent transform must not error: ${f.error}`);
    const parsed = frames.map((f) => runOf(f.result));
    for (const { n } of parsed) assert.equal(n, 1, "one shared execution, not one per waiter");
    for (let i = 1; i < parsed.length; i++) {
      assert.deepEqual(parsed[i].out, parsed[0].out, "waiters get the exact same result object content");
    }
    // No result caching beyond in-flight: a later identical call executes.
    const again = await host.send({ id: 200, hook: "transform", args: [code, mid, "null"] });
    assert.equal(again.error, undefined, `sequential transform must not error: ${again.error}`);
    assert.equal(runOf(again.result).n, 2, "post-settle retry executes the hook again");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("differing code/id/phase/env execute separately", async () => {
  const fx = tmpProject({ prefix: "oj-sf-diff-" });
  const mid = path.join(fx.root, "a.js");
  const mid2 = path.join(fx.root, "b.js");
  const throwId = path.join(fx.root, "throw.js");
  pluginFile(fx, mid, mid2, throwId);
  const host = spawnHost(fx);
  try {
    const codeA = "export const a = 1;\n";
    const codeB = "export const a = 2;\n";
    const r1 = await host.send({ id: 1, hook: "transform", args: [codeA, mid, "null"] });
    assert.equal(runOf(r1.result).n, 1);
    // Same id, different code: separate execution.
    const r2 = await host.send({ id: 2, hook: "transform", args: [codeB, mid, "null"] });
    assert.equal(runOf(r2.result).n, 2, "different code must not join the earlier run");
    assert.ok(runOf(r2.result).out.code.includes("a = 2"), "result tracks the new input, not a stale run");
    // Same code, different id: separate execution.
    const r3 = await host.send({ id: 3, hook: "transform", args: [codeA, mid2, "null"] });
    assert.equal(runOf(r3.result).n, 3, "different id must not join");
    // Same code+id, different phase: a plain hook ranks "normal", so "pre"
    // skips it while "normal" runs it — different results, no joining.
    const pre = await host.send({ id: 4, hook: "transformPhase", args: [codeA, mid, "null", "pre", null] });
    assert.equal(pre.error, undefined, `pre phase must not error: ${pre.error}`);
    assert.equal(JSON.parse(pre.result).code, codeA, "pre phase skips the normal hook");
    const normal = await host.send({ id: 5, hook: "transformPhase", args: [codeA, mid, "null", "normal", null] });
    assert.equal(normal.error, undefined, `normal phase must not error: ${normal.error}`);
    assert.equal(runOf(normal.result).n, 4, "different phase must not join the whole-chain run");
    // Same code+id, different environment: separate execution.
    const opened = await host.send({
      id: 6,
      hook: "openEnvironmentContainer",
      args: [JSON.stringify({ id: 7, name: "ssr" })],
    });
    assert.equal(opened.error, undefined, `opening an ssr container must not error: ${opened.error}`);
    const ssr = await host.send({ id: 8, hook: "transform", args: [codeA, mid, "null"], container: 7 });
    assert.equal(ssr.error, undefined, `ssr transform must not error: ${ssr.error}`);
    assert.equal(runOf(ssr.result).n, 5, "different env must not join the client run");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("a throwing hook rejects all concurrent waiters and a retry executes again", async () => {
  const fx = tmpProject({ prefix: "oj-sf-err-" });
  const mid = path.join(fx.root, "a.js");
  const mid2 = path.join(fx.root, "b.js");
  const throwId = path.join(fx.root, "throw.js");
  pluginFile(fx, mid, mid2, throwId);
  const host = spawnHost(fx);
  try {
    const code = "export const t = 1;\n";
    const frames = await sendConcurrent(
      host,
      Array.from({ length: 4 }, (_, i) => ({ id: 301 + i, hook: "transform", args: [code, throwId, "null"] })),
    );
    for (const f of frames) {
      assert.ok(
        f.error && /singleflight-boom/.test(f.error),
        `every waiter must reject, got: ${JSON.stringify(f).slice(0, 200)}`,
      );
    }
    // The error must not poison the key: a retry executes again and succeeds.
    const retry = await host.send({ id: 400, hook: "transform", args: [code, throwId, "null"] });
    assert.equal(retry.error, undefined, `retry must not error: ${retry.error}`);
    assert.equal(runOf(retry.result).n, 2, "exactly one failed execution plus one retry execution");
  } finally {
    host.close();
    fx.cleanup();
  }
});
