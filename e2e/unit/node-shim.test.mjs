// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { test } from "node:test";
import assert from "node:assert/strict";

import { shimSource, BARE_BUILTINS } from "../../crates/oj_server/src/assets/start/rolldown-assets.mjs";

// The client node-builtin shims mirror Vite's browser-external stubs:
// CommonJS modules, so a named ESM import interops into a property read
// (undefined at runtime) instead of failing the whole bundle with a rolldown
// MISSING_EXPORT at link time. `import { createHmac } from "crypto"` in code
// shared with the server was killing client builds. The dev variant is the
// warning Proxy from Vite's dep optimizer stub, chosen over the throwing
// vite:resolve stub so a feature probe cannot break the page at module eval.

const evalCjs = (src) => {
  const module = { exports: {} };
  new Function("module", "exports", src)(module, module.exports);
  return module.exports;
};

test("default shim is CJS: production is Vite's bare exports object", () => {
  const src = shimSource("node:crypto", true);
  assert.equal(src, "module.exports = {};");
});

test("dev shim warns like Vite's optimizer stub and yields undefined", () => {
  const warnings = [];
  const warn = console.warn;
  console.warn = (m) => warnings.push(m);
  try {
    const exports = evalCjs(shimSource("crypto", false));
    assert.equal(exports.createHmac, undefined);
    assert.equal(warnings.length, 1);
    assert.equal(
      warnings[0],
      'Module "crypto" has been externalized for browser compatibility. Cannot access "crypto.createHmac" in client code.',
    );
    void exports.__esModule;
    assert.equal(warnings.length, 1, "interop __esModule probes must stay silent");
  } finally {
    console.warn = warn;
  }
});

test("a hostile specifier cannot break out of the generated dev shim", () => {
  // The resolveId filter accepts any `node:` tail; the name must reach the
  // generated source only as a string literal, never as code.
  const src = shimSource("node:x`+globalThis.__pwned=1,``${globalThis.__pwned2=1}", false);
  const warn = console.warn;
  console.warn = () => {};
  try {
    const exports = evalCjs(src);
    assert.equal(exports.anything, undefined);
  } finally {
    console.warn = warn;
  }
  assert.equal(globalThis.__pwned, undefined);
  assert.equal(globalThis.__pwned2, undefined);
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

test("bare builtin subpaths are in the shim set like Vite's builtinModules", () => {
  for (const name of ["fs/promises", "timers/promises", "path/posix", "util/types", "stream/web"]) {
    assert.ok(BARE_BUILTINS.test(name), `${name} must be shimmed bare, not only as node:${name}`);
  }
  assert.ok(!BARE_BUILTINS.test("react"), "packages must not match the builtin set");
  assert.ok(!BARE_BUILTINS.test("fs/anything"), "unknown subpaths must not match");
});
