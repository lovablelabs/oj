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

// generateBundle must see a Rollup-shaped bundle: chunks carry facadeModuleId,
// imports, exports and a `modules` map; a plugin can rename a chunk (mutate
// fileName) and delete a chunk (the way @crxjs renames pages and removes its
// manifest JS chunk).
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-gbfid-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "gb-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "dep.js"), `export const dep = 42;\n`);
fs.writeFileSync(
  path.join(app, "src", "entry.js"),
  `import { dep } from "./dep.js";\nexport const v = dep;\nconsole.log(v);\n`,
);
fs.writeFileSync(path.join(app, "src", "keep.js"), `export const keep = "keep-chunk-body";\n`);
fs.writeFileSync(path.join(app, "src", "gone.js"), `export const gone = "gone-chunk-body";\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/entry.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `import path from "node:path";
   import { fileURLToPath } from "node:url";
   const root = path.dirname(fileURLToPath(import.meta.url));
   export default [{
     name: "gb-fidelity",
     buildStart(options) {
       if (typeof options.input !== "undefined") {
         this.emitFile({ type: "chunk", id: path.join(root, "src", "keep.js"), name: "keepme" });
         this.emitFile({ type: "chunk", id: path.join(root, "src", "gone.js"), name: "goneme" });
       }
     },
     generateBundle(options, bundle) {
       let info = null;
       for (const key of Object.keys(bundle)) {
         const c = bundle[key];
         if (c.type === "chunk" && c.isEntry && c.facadeModuleId && c.facadeModuleId.endsWith("entry.js")) {
           info = {
             facadeModuleId: c.facadeModuleId,
             imports: c.imports,
             exports: c.exports,
             moduleCount: Object.keys(c.modules).length,
             hasEntry: Object.keys(c.modules).some((m) => m.endsWith("entry.js")),
             moduleIds: c.moduleIds,
           };
         }
         if (c.type === "chunk" && c.name === "keepme") c.fileName = "assets/renamed-keepme.js";
         if (c.type === "chunk" && c.name === "goneme") delete bundle[key];
       }
       this.emitFile({ type: "asset", fileName: "gb-info.json", source: JSON.stringify(info) });
     },
   }];\n`,
);

let failed = false;
try {
  execSync(`${oj} build ${app}`, { stdio: "ignore" });

  // Read side: the entry chunk exposed a real Rollup shape.
  const info = JSON.parse(fs.readFileSync(path.join(app, "dist", "gb-info.json"), "utf8"));
  assert.ok(info, "the entry chunk was found in generateBundle");
  assert.match(info.facadeModuleId, /entry\.js$/, "facadeModuleId is the entry's source id");
  assert.ok(Array.isArray(info.imports), "chunk.imports is an array");
  assert.ok(Array.isArray(info.exports), "chunk.exports is an array");
  assert.ok(info.exports.includes("v"), "chunk.exports lists the entry's export");
  assert.ok(info.moduleCount >= 1, "chunk.modules is a populated map keyed by module id");
  assert.ok(info.hasEntry, "chunk.modules includes the entry module id");
  assert.ok(Array.isArray(info.moduleIds) && info.moduleIds.length >= 1, "chunk.moduleIds is populated");

  // Rename side: the keepme chunk was written under its new name.
  const renamed = path.join(app, "dist", "assets", "renamed-keepme.js");
  assert.ok(fs.existsSync(renamed), "renamed chunk exists at its new fileName");
  assert.match(fs.readFileSync(renamed, "utf8"), /keep-chunk-body/, "renamed chunk keeps its content");

  // Delete side: the goneme chunk is absent from the output entirely.
  const allJs = [];
  const walk = (d) => {
    for (const e of fs.readdirSync(d, { withFileTypes: true })) {
      const p = path.join(d, e.name);
      if (e.isDirectory()) walk(p);
      else if (e.name.endsWith(".js")) allJs.push(fs.readFileSync(p, "utf8"));
    }
  };
  walk(path.join(app, "dist"));
  assert.ok(!allJs.some((c) => c.includes("gone-chunk-body")), "deleted chunk was not written");

  console.log("GENERATE-BUNDLE-FIDELITY E2E PASSED");
} catch (err) {
  failed = true;
  console.error("GENERATE-BUNDLE-FIDELITY E2E FAILED:", err.message || err);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
