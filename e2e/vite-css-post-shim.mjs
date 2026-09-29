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

// UnoCSS-style plugins find a `vite:css-post` plugin in config.plugins and, in
// renderChunk, hand it their generated CSS to route into the output. This test
// mimics that pattern (no UnoCSS dependency): a virtual `.css` layer module kept
// as a stub, then renderChunk hands generated CSS to oj's vite:css-post shim.
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-csspost-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "csspost-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "entry.js"), `import "virtual:layer.css";\ndocument.body.className = "gen";\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/entry.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `let allPlugins;
   export default [{
     name: "fake-uno",
     configResolved(config) { allPlugins = config.plugins; },
     resolveId(id) { return id === "virtual:layer.css" ? "\\0virtual:layer.css" : null; },
     load(id) { return id === "\\0virtual:layer.css" ? "/* layer placeholder */" : null; },
     renderChunk(code, chunk) {
       const hasLayer = Object.keys(chunk.modules || {}).some((m) => m.includes("layer.css"));
       if (!hasLayer) return null;
       const cssPost = (allPlugins || []).find((p) => p && p.name === "vite:css-post");
       if (!cssPost) throw new Error("vite:css-post shim not found in config.plugins");
       const handler = typeof cssPost.transform === "function" ? cssPost.transform : cssPost.transform.handler;
       handler.call(this, ".gen{color:green}", "\\0virtual:layer.css");
       return null;
     },
   }];\n`,
);

let failed = false;
try {
  execSync(`${oj} build ${app}`, { stdio: "pipe" });

  const cssDir = path.join(app, "dist", "assets");
  const css = fs.existsSync(cssDir)
    ? fs
        .readdirSync(cssDir)
        .filter((f) => f.endsWith(".css"))
        .map((f) => fs.readFileSync(path.join(cssDir, f), "utf8"))
        .join("\n")
    : "";

  assert.ok(css.includes(".gen"), "CSS handed to the vite:css-post shim reached the output stylesheet");
  const html = fs.readFileSync(path.join(app, "dist", "index.html"), "utf8");
  assert.match(html, /rel="stylesheet"/, "the page links the generated stylesheet");

  console.log("VITE-CSS-POST-SHIM E2E PASSED");
} catch (err) {
  failed = true;
  console.error("VITE-CSS-POST-SHIM E2E FAILED:", (err.stderr && err.stderr.toString()) || err.message || err);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
