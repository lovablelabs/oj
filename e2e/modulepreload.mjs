// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Verifies the production build injects <link rel="modulepreload"> for an
// entry's statically-imported chunks. manualChunks forces a vendor chunk that
// the entry statically imports, which must then be preloaded from the HTML.

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const OJ = path.join(process.cwd(), "target", "debug", "oj");
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-preload-"));

let failed = false;
try {
  fs.mkdirSync(path.join(app, "src"), { recursive: true });
  fs.mkdirSync(path.join(app, "node_modules", "mylib"), { recursive: true });
  fs.writeFileSync(
    path.join(app, "package.json"),
    '{"name":"preload","private":true,"dependencies":{"mylib":"1.0.0"}}',
  );
  fs.writeFileSync(
    path.join(app, "node_modules", "mylib", "package.json"),
    '{"name":"mylib","version":"1.0.0","type":"module","main":"index.js"}',
  );
  fs.writeFileSync(path.join(app, "node_modules", "mylib", "index.js"), "export const hello = 'hi from mylib';\n");
  fs.writeFileSync(
    path.join(app, "index.html"),
    '<!doctype html><html><head></head><body><script type="module" src="/src/main.tsx"></script></body></html>',
  );
  fs.writeFileSync(path.join(app, "src", "main.tsx"), 'import { hello } from "mylib";\nwindow.__h = hello;\n');
  // Split mylib into its own chunk that the entry statically imports.
  fs.writeFileSync(
    path.join(app, "oj.config.mjs"),
    'export default { build: { rollupOptions: { output: { manualChunks: { vendor: ["mylib"] } } } } };\n',
  );

  execFileSync(OJ, ["build", app], { stdio: "pipe" });

  const files = fs.readdirSync(path.join(app, "dist", "assets"));
  const html = fs.readFileSync(path.join(app, "dist", "index.html"), "utf8");
  const links = [...html.matchAll(/<link rel="modulepreload" href="([^"]+)"/g)].map((m) => m[1]);
  if (!links.length) throw new Error("no modulepreload links injected. chunks: " + files.join(", ") + "\n" + html);
  for (const href of links) {
    const f = path.join(app, "dist", href.replace(/^\//, ""));
    if (!fs.existsSync(f)) throw new Error("preloaded chunk missing on disk: " + href);
  }
  if (!links.some((h) => /vendor-/.test(h))) throw new Error("vendor chunk not preloaded: " + links.join(", "));
  if (!/<link rel="modulepreload" href="[^"]+" crossorigin/.test(html))
    throw new Error("modulepreload links lack crossorigin (Vite sets it)\n" + html);
  console.log("chunks:             ", files.filter((f) => f.endsWith(".js")).join(", "));
  console.log("modulepreload links:", links.join(", "));
  console.log("\nMODULEPRELOAD VERIFIED: entry static-import chunks preloaded in HTML");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
