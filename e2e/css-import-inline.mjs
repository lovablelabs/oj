// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Plain `@import`s are inlined like Vite's postcss-import step (relative chains,
// bare node_modules packages via their `style` entry) with url() rebased to the
// entry, in dev and in the build; media-query imports stay. In the build, CSS
// url() assets follow build.assetsInlineLimit (small files become data URLs)
// and output.assetFileNames names the rest and the chunk stylesheets.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const PORT = 5542;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-cssimp-"));
fs.mkdirSync(path.join(app, "src", "base"), { recursive: true });
fs.mkdirSync(path.join(app, "node_modules", "normalize-fake"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "cssimp-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "vite.config.js"),
  `export default { build: { assetsInlineLimit: 1024, rollupOptions: { output: { assetFileNames: "static/[name].[hash][extname]" } } } };\n`,
);
fs.writeFileSync(
  path.join(app, "node_modules", "normalize-fake", "package.json"),
  JSON.stringify({ name: "normalize-fake", style: "n.css" }),
);
fs.writeFileSync(path.join(app, "node_modules", "normalize-fake", "n.css"), `.norm { margin: 0; }\n`);
fs.writeFileSync(path.join(app, "src", "vars.css"), `:root { --x: 1; }\n`);
fs.writeFileSync(path.join(app, "src", "base", "dot.png"), Buffer.alloc(100, 7));
fs.writeFileSync(path.join(app, "src", "base", "big.png"), Buffer.alloc(5000, 9));
fs.writeFileSync(
  path.join(app, "src", "base", "reset.css"),
  `@import "../vars.css";\n.reset { background: url(./dot.png); }\n.big { background: url("./big.png"); }\n`,
);
fs.writeFileSync(
  path.join(app, "src", "app.css"),
  `@import "./base/reset.css";\n@import 'normalize-fake';\n@import "./print.css" print;\n.app { color: red; }\n`,
);
fs.writeFileSync(path.join(app, "src", "print.css"), `.print { display: none; }\n`);
fs.writeFileSync(path.join(app, "src", "main.js"), `import "./app.css";\nwindow.__OK = true;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${PORT}/`);
  const css = await (await fetch(`http://localhost:${PORT}/src/app.css?direct`)).text();
  assert.match(css, /--x:\s*1/, `nested relative import inlined:\n${css}`);
  assert.match(css, /\.reset/, "relative import inlined");
  assert.match(css, /\.norm/, "bare package import inlined via style entry");
  assert.match(css, /url\(["']?\/src\/base\/dot\.png/, `inlined file's url() rebased to the server root:\n${css}`);
  assert.match(css, /@media print\s*\{[^}]*\.print/, `media-query import inlined as @media:\n${css}`);
  assert.doesNotMatch(css, /@import\s+["']\.\/base|@import\s+['"]normalize-fake/, "inlined imports still present");
  console.log("[dev] @import inlining OK");
  srv.kill("SIGKILL");
  await sleep(300);

  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const dist = path.join(app, "dist");
  const staticDir = path.join(dist, "static");
  assert.ok(fs.existsSync(staticDir), "assetFileNames directory used");
  const files = fs.readdirSync(staticDir);
  const cssFile = files.find((f) => f.endsWith(".css"));
  assert.ok(cssFile, `chunk stylesheet named by assetFileNames: ${files}`);
  assert.match(cssFile, /^main\.[0-9a-f]{8}\.css$/, `stylesheet name follows [name].[hash][extname]: ${cssFile}`);
  const built = fs.readFileSync(path.join(staticDir, cssFile), "utf8");
  assert.match(built, /\.norm/, "package import inlined in build");
  assert.match(built, /--x:\s*1/, "nested import inlined in build");
  assert.match(built, /data:image\/png;base64,/, `100-byte png inlined under assetsInlineLimit:\n${built}`);
  const big = files.find((f) => /^big\.[0-9a-f]{8}\.png$/.test(f));
  assert.ok(big, `5000-byte png emitted by assetFileNames pattern: ${files}`);
  assert.match(built, new RegExp(`static/${big.replace(/\./g, "\\.")}`), "css references the emitted asset path");
  assert.doesNotMatch(built, /@import\s+["']\.\//, "relative imports left in build css");
  assert.match(built, /@media print\{\.print/, `media import inlined as @media in build:\n${built}`);
  console.log("[build] @import inlining, inline limit, assetFileNames OK");
  console.log("CSS-IMPORT-INLINE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("CSS-IMPORT-INLINE E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
