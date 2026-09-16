// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Vite's ctx.resolve threads a cumulative `skipCalls` list ({ id, importer,
// plugin, called }) through the whole nested resolve chain: a plugin is skipped
// for the same id + importer it already issued, and outright once it re-issues
// an identical call (`called`). A skip covering only the direct caller resets
// the exclusion at every hop, so two plugins that both this.resolve the same id
// with skipSelf re-enter each other forever (unbounded recursion, host OOM).
import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { repo, rpcSidecar, tmpProject } from "./harness.mjs";

// Two plugins that both this.resolve({ skipSelf: true }) the same `?url` id
// from resolveId -- the shape that loops under a caller-only skip. `counts`
// reports how often each hook ran; the loop bound turns a regression into a
// fast hook error instead of a hang.
const mutualPlugins = `
let stubs = 0, protection = 0;
export default [
  {
    name: "stubs",
    async resolveId(source, importer) {
      if (source === "counts") return "\\0counts:" + JSON.stringify({ stubs, protection });
      if (!source.endsWith("?url")) return null;
      if (++stubs > 5) throw new Error("resolve loop (stubs)");
      const r = await this.resolve(source, importer, { skipSelf: true });
      return r ? "\\0url:" + r.id : null;
    },
  },
  {
    name: "protection",
    async resolveId(source, importer) {
      if (source.startsWith("\\0") || source === "counts") return null;
      if (++protection > 5) throw new Error("resolve loop (protection)");
      await this.resolve(source, importer, { skipSelf: true });
      return null;
    },
  },
];
`;

// Vite scopes each skipCalls entry to the issued id + importer: `a` issues
// alias:two, so deeper in that chain `a` still answers for alias:three. A
// blanket "skip every caller in the chain for any id" would silence it.
const rewritePlugins = `
export default [
  {
    name: "a",
    async resolveId(source, importer) {
      if (source === "alias:one") {
        const r = await this.resolve("alias:two", importer, { skipSelf: true });
        return r ? "\\0one:" + r.id : null;
      }
      if (source === "alias:three") return "\\0three";
      return null;
    },
  },
  {
    name: "b",
    async resolveId(source, importer) {
      if (source !== "alias:two") return null;
      const r = await this.resolve("alias:three", importer, { skipSelf: true });
      return r ? "\\0two:" + r.id : null;
    },
  },
];
`;

// Two plugins rewriting into each other (ping resolves pong, pong resolves
// ping): the same-id skip alone never matches, only the `called` hard-stop
// terminates the chain. Vite converges in two visits per hook.
const pingPongPlugins = `
let ping = 0, pong = 0;
export default [
  {
    name: "ping",
    async resolveId(source, importer) {
      if (source === "counts") return "\\0counts:" + JSON.stringify({ ping, pong });
      if (source !== "ping") return null;
      if (++ping > 5) throw new Error("resolve loop (ping)");
      await this.resolve("pong", importer, { skipSelf: true });
      return null;
    },
  },
  {
    name: "pong",
    async resolveId(source, importer) {
      if (source !== "pong") return null;
      if (++pong > 5) throw new Error("resolve loop (pong)");
      await this.resolve("ping", importer, { skipSelf: true });
      return null;
    },
  },
];
`;

// Drive one hook to completion, answering the host's ctx RPCs like the Rust
// side would: the disk resolver knows asset.css and nothing else.
async function drive(host, fx, msg) {
  let frame = await host.send(msg);
  while (frame.rpc != null) {
    let result = null;
    if (frame.method === "resolve" && frame.args[0] === "asset.css?url") result = path.join(fx.root, "asset.css");
    host.child.stdin.write(JSON.stringify({ rpcReply: frame.rpc, result }) + "\n");
    frame = await host.nextFrame();
  }
  return frame;
}

function hostFor(fx) {
  return rpcSidecar("plugin-host.mjs", {
    args: [path.join(fx.root, "oj.plugins.mjs"), JSON.stringify({ root: fx.root })],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
}

async function counts(host, fx) {
  const frame = await drive(host, fx, { id: 99, hook: "resolveId", args: ["counts", path.join(fx.root, "main.js")] });
  return JSON.parse(frame.result.slice("\0counts:".length));
}

test("two plugins mutually resolving the same id with skipSelf terminate (Vite skipCalls)", async () => {
  const fx = tmpProject({ prefix: "oj-skipcalls-" });
  fx.write("asset.css", "body{}\n");
  fx.write("oj.plugins.mjs", mutualPlugins);
  const host = hostFor(fx);
  try {
    const importer = path.join(fx.root, "main.js");
    const frame = await drive(host, fx, { id: 1, hook: "resolveId", args: ["asset.css?url", importer] });
    assert.equal(frame.error, undefined, `resolveId must not fail: ${frame.error}; stderr:\n${host.stderr()}`);
    assert.equal(frame.result, "\0url:" + path.join(fx.root, "asset.css"), "the outer call still resolves through the chain");
    const c = await counts(host, fx);
    assert.equal(c.stubs, 1, "the issuing plugin is skipped for its own id across the whole nested chain");
    assert.equal(c.protection, 1, "the sibling runs once, and its nested resolve skips both");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("a skipCalls entry is scoped to its issued id: the issuer still answers deeper in the chain", async () => {
  const fx = tmpProject({ prefix: "oj-skipscope-" });
  fx.write("oj.plugins.mjs", rewritePlugins);
  const host = hostFor(fx);
  try {
    const frame = await drive(host, fx, { id: 1, hook: "resolveId", args: ["alias:one", path.join(fx.root, "main.js")] });
    assert.equal(frame.error, undefined, `resolveId must not fail: ${frame.error}; stderr:\n${host.stderr()}`);
    assert.equal(frame.result, "\0one:\0two:\0three", "`a` answers alias:three inside the chain it started");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("mutually rewriting plugins terminate via the re-issued-call hard-stop", async () => {
  const fx = tmpProject({ prefix: "oj-skipping-" });
  fx.write("oj.plugins.mjs", pingPongPlugins);
  const host = hostFor(fx);
  try {
    const frame = await drive(host, fx, { id: 1, hook: "resolveId", args: ["ping", path.join(fx.root, "main.js")] });
    assert.equal(frame.error, undefined, `resolveId must not fail: ${frame.error}; stderr:\n${host.stderr()}`);
    const c = await counts(host, fx);
    // Exact Vite parity: rolldown-vite 8.2.1 runs each hook twice for this
    // shape (verified empirically against a real createServer).
    assert.deepEqual(c, { ping: 2, pong: 2 }, "two visits per hook, exactly like Vite");
  } finally {
    host.close();
    fx.cleanup();
  }
});

// The START container's plugin bridge has its own ctx.resolve; same contract.
const bridge = await import(
  pathToFileURL(path.join(repo, "crates/oj_server/src/assets/start/vite-plugin-bridge.mjs")).href
);

test("bridge: mutual skipSelf on the same id terminates and still resolves", async () => {
  let stubs = 0;
  let protection = 0;
  const plugins = [
    {
      name: "stubs",
      async resolveId(source, importer) {
        if (!source.endsWith("?url")) return null;
        if (++stubs > 5) throw new Error("resolve loop (stubs)");
        const r = await this.resolve(source, importer, { skipSelf: true });
        return r ? "\0url:" + r.id : null;
      },
    },
    {
      name: "protection",
      async resolveId(source, importer) {
        if (source.startsWith("\0")) return null;
        if (++protection > 5) throw new Error("resolve loop (protection)");
        await this.resolve(source, importer, { skipSelf: true });
        return null;
      },
    },
    {
      name: "disk",
      resolveId(source) {
        return source === "asset.css?url" ? "/repo/asset.css?url" : null;
      },
    },
  ];
  const container = bridge.createPluginContainer({}, plugins, {
    command: "serve",
    environment: "ssr",
    config: { root: repo, environments: { client: {}, ssr: {} } },
  });
  const id = await container.resolveId("asset.css?url", "/repo/main.js");
  assert.equal(id, "\0url:/repo/asset.css?url");
  assert.equal(stubs, 1, "the issuing plugin is skipped for its own id across the nested chain");
  assert.equal(protection, 1, "the sibling runs once, and its nested resolve skips both");
});

test("bridge: a skipCalls entry is scoped to its issued id", async () => {
  const plugins = [
    {
      name: "a",
      async resolveId(source, importer) {
        if (source === "alias:one") {
          const r = await this.resolve("alias:two", importer, { skipSelf: true });
          return r ? "\0one:" + r.id : null;
        }
        if (source === "alias:three") return "\0three";
        return null;
      },
    },
    {
      name: "b",
      async resolveId(source, importer) {
        if (source !== "alias:two") return null;
        const r = await this.resolve("alias:three", importer, { skipSelf: true });
        return r ? "\0two:" + r.id : null;
      },
    },
  ];
  const container = bridge.createPluginContainer({}, plugins, {
    command: "serve",
    environment: "ssr",
    config: { root: repo, environments: { client: {}, ssr: {} } },
  });
  assert.equal(await container.resolveId("alias:one", "/repo/main.js"), "\0one:\0two:\0three");
});
