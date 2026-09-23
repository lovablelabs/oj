// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Verifies the plugin-served module request cache: a warm /@id/ re-request
// serves identical bytes with an ETag, If-None-Match answers 304, and a plugin
// whose load output changed (its source is read from a file the test edits)
// serves the new module with a new ETag instead of the stale compile.

import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";

const OJ = path.join(process.cwd(), "target", "debug", "oj");
const PORT = 5346;
const base = `http://127.0.0.1:${PORT}`;
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-plugin-cache-"));
const greeting = path.join(app, "greeting.txt");

let failed = false;
let child;
try {
  fs.mkdirSync(path.join(app, "src"), { recursive: true });
  fs.writeFileSync(path.join(app, "package.json"), '{"name":"plugin-cache","private":true}');
  fs.writeFileSync(greeting, "hello-v1\n");
  fs.writeFileSync(
    path.join(app, "src", "entry.js"),
    'import { greeting } from "virtual:greeting";\nwindow.__g = greeting;\n',
  );
  fs.writeFileSync(
    path.join(app, "index.html"),
    '<!doctype html><html><head></head><body><script type="module" src="/src/entry.js"></script></body></html>',
  );
  fs.writeFileSync(
    path.join(app, "oj.plugins.mjs"),
    `import fs from "node:fs";
     const data = ${JSON.stringify(greeting)};
     export default [{
       name: "greeting-virtual",
       resolveId(id) { return id === "virtual:greeting" ? "\\0virtual:greeting" : null; },
       load(id) {
         if (id !== "\\0virtual:greeting") return null;
         const txt = fs.readFileSync(data, "utf8").trim();
         return \`export const greeting = \${JSON.stringify(txt)};\`;
       },
     }];\n`,
  );

  child = spawn(OJ, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
  let up = false;
  for (let i = 0; i < 300; i++) {
    try { if ((await fetch(`${base}/`)).ok) { up = true; break; } } catch {}
    await new Promise((r) => setTimeout(r, 100));
  }
  assert.ok(up, "dev server came up");

  const entry = await (await fetch(`${base}/src/entry.js`)).text();
  const idUrl = entry.match(/\/@id\/[a-f0-9]+\?importer=[a-f0-9]+/)?.[0];
  assert.ok(idUrl, "the bare specifier was rewritten to a /@id/ URL:\n" + entry);

  const first = await fetch(`${base}${idUrl}`);
  assert.equal(first.status, 200);
  const etag = first.headers.get("etag");
  assert.ok(etag, "plugin module response carries an ETag");
  const body1 = await first.text();
  assert.match(body1, /hello-v1/, "plugin load output served:\n" + body1);

  const second = await fetch(`${base}${idUrl}`);
  assert.equal(await second.text(), body1, "warm re-request differs");
  assert.equal(second.headers.get("etag"), etag, "warm re-request changed the ETag");
  console.log("warm re-request:  identical, stable ETag", etag);

  const conditional = await fetch(`${base}${idUrl}`, { headers: { "if-none-match": etag } });
  assert.equal(conditional.status, 304, `If-None-Match should 304, got ${conditional.status}`);
  console.log("if-none-match:    304");

  // The plugin's load now returns different source; the source-hash key must
  // recompile even against the old validator.
  fs.writeFileSync(greeting, "hello-v2-changed\n");
  let fresh;
  for (let i = 0; i < 100; i++) {
    fresh = await fetch(`${base}${idUrl}`, { headers: { "if-none-match": etag } });
    if (fresh.status === 200) break;
    await new Promise((r) => setTimeout(r, 100));
  }
  assert.equal(fresh.status, 200, "changed plugin source must not answer 304");
  const body2 = await fresh.text();
  assert.match(body2, /hello-v2-changed/, "recompiled module missing new source:\n" + body2);
  assert.doesNotMatch(body2, /hello-v1/, "stale compile served after plugin source change");
  assert.notEqual(fresh.headers.get("etag"), etag, "ETag did not change with the source");
  console.log("changed source:   recompiled, new ETag", fresh.headers.get("etag"));
  console.log("\nPLUGIN REQUEST CACHE VERIFIED");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message || e);
} finally {
  if (child) child.kill("SIGKILL");
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
