// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { stripTypeScriptTypes } from "node:module";
import { pathToFileURL } from "node:url";
import { asset, repo } from "./harness.mjs";

// oj's stand-in for `@tanstack/react-start/server-entry` hands Start's handler
// every argument its `fetch` was called with, as the package's own entry does
// (dist/default-entry/esm/server.js: `createServerEntry({ fetch })` over the
// handler, `fetch(...args)` passed on whole). Start's handler reads only the
// second (its request options, `{ context }`), so the parity is not
// observable through Start; the module is run here over a stub handler that
// records its arguments. The one divergence is deliberate: a Worker runtime's
// `fetch(request, env, ctx)`, told by the execution context, stashes the
// bindings for `getCloudflareContext()` and calls the handler with the request
// alone, as the bindings are not the handler's options.

const startEntry = path.join(
  repo,
  "e2e/fixtures/start-app/node_modules/@tanstack/react-start/dist/default-entry/esm/server.js",
);

/// oj's entry module, its TypeScript stripped and its import of
/// `@tanstack/react-start/server` pointed at a stub whose handler records
/// every call's arguments.
async function loadEntry() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "oj-server-entry-"));
  const calls = [];
  globalThis.__ojEntryCalls = calls;
  fs.writeFileSync(
    path.join(dir, "start-server.mjs"),
    `export const defaultStreamHandler = { stub: true };
export function createStartHandler(stream) {
  if (stream !== defaultStreamHandler) throw new Error("the handler is not over the default stream handler");
  return (...args) => {
    globalThis.__ojEntryCalls.push(args);
    return new Response("ok");
  };
}
`,
  );
  const source = fs.readFileSync(asset("start/server-entry.tsx"), "utf8");
  const stripped = stripTypeScriptTypes(source, { mode: "strip" }).replace(
    '"@tanstack/react-start/server"',
    JSON.stringify(pathToFileURL(path.join(dir, "start-server.mjs")).href),
  );
  assert.notEqual(stripped, source, "the import of @tanstack/react-start/server was not redirected");
  const file = path.join(dir, "server-entry.mjs");
  fs.writeFileSync(file, stripped);
  const mod = await import(pathToFileURL(file).href);
  return { mod, calls, cleanup: () => fs.rmSync(dir, { recursive: true, force: true }) };
}

test("the entry's fetch passes every argument on to Start's handler, as the package's entry does", async () => {
  const { mod, calls, cleanup } = await loadEntry();
  try {
    const request = new Request("http://app.test/");
    const options = { context: { nonce: "n-1" } };
    const extra = { not: "an execution context" };

    // As Vite's dev middleware calls it: the request alone.
    await mod.default.fetch(request);
    // As an app's entry calls it: the request and its options.
    await mod.default.fetch(request, options);
    // A third argument that is no execution context goes on too.
    await mod.default.fetch(request, options, extra);

    assert.equal(calls.length, 3);
    assert.deepEqual(calls[0].length, 1);
    assert.equal(calls[0][0], request);
    assert.deepEqual(calls[1].length, 2);
    assert.equal(calls[1][1], options, "the options object itself, not a copy");
    assert.deepEqual(calls[2].length, 3);
    assert.equal(calls[2][1], options);
    assert.equal(calls[2][2], extra);

    // The package's `createServerEntry` forwards the same way: the same
    // arity for the same calls.
    if (fs.existsSync(startEntry)) {
      const { createServerEntry } = await import(pathToFileURL(startEntry).href);
      const seen = [];
      const theirs = createServerEntry({ fetch: (...args) => (seen.push(args), new Response("ok")) });
      await theirs.fetch(request);
      await theirs.fetch(request, options);
      await theirs.fetch(request, options, extra);
      assert.deepEqual(
        seen.map((args) => args.length),
        calls.map((args) => args.length),
        "oj's entry and the package's forward a different number of arguments",
      );
    }
  } finally {
    cleanup();
  }
});

test("a Worker's fetch(request, env, ctx) stashes the bindings and calls the handler with the request alone", async () => {
  const { mod, calls, cleanup } = await loadEntry();
  try {
    const request = new Request("http://app.test/");
    const env = { KV: { binding: true } };
    const ctx = { waitUntil() {}, passThroughOnException() {} };
    await mod.default.fetch(request, env, ctx);
    assert.equal(calls.length, 1);
    assert.deepEqual(calls[0], [request], "a Worker's bindings are not the handler's options");
    assert.equal(globalThis.__OJ_CF_ENV, env);
    assert.equal(globalThis.__OJ_CF_CTX, ctx);
  } finally {
    delete globalThis.__OJ_CF_ENV;
    delete globalThis.__OJ_CF_CTX;
    cleanup();
  }
});

test("oj's createServerEntry forwards every argument, as the package's does", async () => {
  const { mod, cleanup } = await loadEntry();
  try {
    const seen = [];
    const entry = mod.createServerEntry({ fetch: (...args) => (seen.push(args), new Response("ok")) });
    const request = new Request("http://app.test/");
    const a = {};
    const b = {};
    await entry.fetch(request, a, b);
    assert.deepEqual(seen, [[request, a, b]]);
  } finally {
    cleanup();
  }
});
