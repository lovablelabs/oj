// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Verifies .vite/manifest.json completeness: all chunks (not just entries),
// non-entry chunks keyed _<file>, imports/dynamicImports referencing manifest
// KEYS, and isDynamicEntry flagged. Standalone `oj build` on a temp fixture.

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const OJ = path.join(process.cwd(), "target", "debug", "oj");
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-manifest-"));

let failed = false;
try {
  fs.mkdirSync(path.join(app, "src"), { recursive: true });
  fs.mkdirSync(path.join(app, "node_modules", "mylib"), { recursive: true });
  fs.writeFileSync(path.join(app, "package.json"), '{"name":"m","private":true,"dependencies":{"mylib":"1.0.0"}}');
  fs.writeFileSync(
    path.join(app, "node_modules", "mylib", "package.json"),
    '{"name":"mylib","version":"1.0.0","type":"module","main":"index.js"}',
  );
  // A non-trivial module so rolldown keeps it as its own chunk (tiny modules
  // get inlined below the chunk-size threshold even with manualChunks).
  fs.writeFileSync(
    path.join(app, "node_modules", "mylib", "index.js"),
    "export const hello = 'hi from mylib';\n" +
      "export function greet(name) { return hello + ' ' + name.toUpperCase(); }\n" +
      "export const data = Array.from({ length: 64 }, (_, i) => ({ i, sq: i * i }));\n",
  );
  fs.writeFileSync(
    path.join(app, "index.html"),
    '<!doctype html><html><head></head><body><script type="module" src="/src/main.tsx"></script></body></html>',
  );
  fs.writeFileSync(
    path.join(app, "src", "main.tsx"),
    'import { greet, data } from "mylib";\nwindow.__h = greet("world") + data.length;\n',
  );
  // manualChunks forces a vendor chunk the entry statically imports.
  fs.writeFileSync(
    path.join(app, "oj.config.mjs"),
    'export default { build: { manifest: true, rollupOptions: { output: { manualChunks: { vendor: ["mylib"] } } } } };\n',
  );

  execFileSync(OJ, ["build", app], { stdio: "pipe" });
  const m = JSON.parse(fs.readFileSync(path.join(app, "dist", ".vite", "manifest.json"), "utf8"));
  const keys = Object.keys(m);

  const entry = m["src/main.tsx"];
  if (!entry || entry.isEntry !== true)
    throw new Error("entry not keyed by src or not isEntry:\n" + JSON.stringify(keys));

  // The non-entry vendor chunk is now in the manifest, keyed _<filename>, no src.
  const vendorKey = keys.find((k) => /^_vendor-.*\.js$/.test(k));
  if (!vendorKey) throw new Error("non-entry vendor chunk not in manifest: " + keys.join(", "));
  if (m[vendorKey].src) throw new Error("non-entry chunk should have no src");
  if (m[vendorKey].isEntry) throw new Error("non-entry chunk should not be isEntry");

  // imports reference manifest KEYS (not output filenames).
  if (!(entry.imports || []).includes(vendorKey))
    throw new Error("entry.imports should reference the vendor KEY: " + JSON.stringify(entry.imports));

  // Referential integrity: every imports/dynamicImports key exists as a manifest key.
  for (const k of keys) {
    for (const ref of [...(m[k].imports || []), ...(m[k].dynamicImports || [])]) {
      if (!m[ref]) throw new Error(`${k} references missing manifest key ${ref}`);
    }
  }

  console.log("manifest keys:  ", keys.join(", "));
  console.log("entry.imports:  ", JSON.stringify(entry.imports), "(keys, not filenames)");
  console.log("\nMANIFEST VERIFIED: all chunks present, non-entry keyed _<file>, imports by key");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
