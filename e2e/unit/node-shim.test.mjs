// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";

import { shimSource } from "../../crates/oj_server/src/assets/start/rolldown-assets.mjs";

// The client node-builtin shims mirror Vite's browser-external stubs: CommonJS
// modules, so a named ESM import interops into a property read (undefined at
// runtime) instead of failing the whole bundle with a rolldown MISSING_EXPORT
// at link time — `import { createHmac } from "crypto"` in code shared with the
// server was killing client builds (fingerprint 9322bab6 family, 38 projects
// in the 2026-09-27 5k campaign).

test("default shim is CJS: production is Vite's bare exports object", () => {
  const src = shimSource("node:crypto", true);
  assert.equal(src, "module.exports = {};");
});

test("default shim is CJS: dev wraps Vite's warning proxy, naming the module", () => {
  const src = shimSource("crypto", false);
  assert.match(src, /^module\.exports = Object\.create\(new Proxy\(/);
  assert.match(src, /Module "crypto" has been externalized for browser compatibility/);
  assert.match(src, /Cannot access "crypto\.\$\{key\}"/);
});

test("bespoke shims stay CJS so uncovered names interop instead of link-failing", () => {
  for (const name of ["stream", "stream/web", "punycode", "async_hooks"]) {
    const src = shimSource(name, true);
    assert.match(src, /module\.exports=\{/, `${name} shim must be CommonJS`);
    assert.doesNotMatch(src, /^export /m, `${name} shim must not use ESM exports`);
  }
});

test("bespoke stream shim still carries its working classes", () => {
  const src = shimSource("node:stream", false);
  for (const cls of ["Readable", "Writable", "Duplex", "Transform", "PassThrough", "Stream"]) {
    assert.match(src, new RegExp(`\\b${cls}\\b`));
  }
});
