// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

// The Vite build manifest must be present in the generateBundle bundle as a
// `.vite/manifest.json` asset, so plugins that read it there (e.g. @crxjs's
// web-accessible-resources) can. Here a plugin reads it and re-emits it.
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-vitemanifest-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "vm-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "entry.js"), `export const v = 1;\nconsole.log(v);\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/entry.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `export default [{
     name: "reads-vite-manifest",
     generateBundle(options, bundle) {
       const m = bundle[".vite/manifest.json"];
       if (m && m.type === "asset" && typeof m.source === "string") {
         this.emitFile({ type: "asset", fileName: "seen-manifest.json", source: m.source });
       }
     },
   }];\n`,
);

let failed = false;
try {
  execSync(`${oj} build ${app}`, { stdio: "pipe" });

  const seenPath = path.join(app, "dist", "seen-manifest.json");
  assert.ok(fs.existsSync(seenPath), "the plugin read .vite/manifest.json from the generateBundle bundle");
  const manifest = JSON.parse(fs.readFileSync(seenPath, "utf8"));
  const entry = Object.values(manifest).find((e) => e.isEntry);
  assert.ok(entry, "the manifest lists an entry");
  assert.match(entry.file, /\.js$/, "the entry maps to a js chunk");

  console.log("VITE-BUILD-MANIFEST-IN-BUNDLE E2E PASSED");
} catch (err) {
  failed = true;
  console.error(
    "VITE-BUILD-MANIFEST-IN-BUNDLE E2E FAILED:",
    (err.stderr && err.stderr.toString()) || err.message || err,
  );
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
