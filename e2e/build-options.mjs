// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite build options oj used to reject or ignore: `build.sourcemap` "hidden" /
// "inline", `build.minify` as a minifier name, `build.target` presets and
// arrays, and `build.emptyOutDir` (an outDir outside the project root is not
// wiped unless asked; `.git` inside outDir survives).

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

const base = fs.mkdtempSync(path.join(os.tmpdir(), "oj-buildopts-"));
const app = path.join(base, "app");
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "buildopts", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "main.js"), `const o = { a: { b: 1 } };\nwindow.__V = o?.a?.b ?? 0;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

function build(config, extraArgs = "") {
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  fs.rmSync(path.join(app, "oj.config.json"), { force: true });
  fs.rmSync(path.join(app, "vite.config.js"), { force: true });
  if (typeof config === "string") fs.writeFileSync(path.join(app, "vite.config.js"), config);
  else fs.writeFileSync(path.join(app, "oj.config.json"), JSON.stringify(config));
  return execSync(`${oj} build ${app} ${extraArgs}`, { stdio: ["ignore", "pipe", "pipe"] }).toString();
}
function mainChunk(outDir = path.join(app, "dist")) {
  const assets = path.join(outDir, "assets");
  const js = fs.readdirSync(assets).find((f) => f.startsWith("main-") && f.endsWith(".js"));
  assert.ok(js, "main chunk emitted");
  return { code: fs.readFileSync(path.join(assets, js), "utf8"), map: path.join(assets, js + ".map") };
}

let failed = false;
try {
  // sourcemap: "hidden" writes the map but no sourceMappingURL comment.
  build({ build: { sourcemap: "hidden" } });
  let m = mainChunk();
  assert.ok(fs.existsSync(m.map), "hidden: .map written");
  assert.doesNotMatch(m.code, /sourceMappingURL/, "hidden: no sourceMappingURL comment");

  // sourcemap: "inline" embeds a data URL and writes no .map file.
  build({ build: { sourcemap: "inline" } });
  m = mainChunk();
  assert.ok(!fs.existsSync(m.map), "inline: no .map file");
  assert.match(m.code, /sourceMappingURL=data:application\/json/, "inline: data url comment");

  // minify as a minifier name means "on"; false keeps whitespace.
  build({ build: { minify: "terser", terserOptions: { compress: {} } } });
  assert.doesNotMatch(mainChunk().code, /\n\s+window/, "minify: 'terser' minifies");
  build({ build: { minify: false } });
  assert.match(mainChunk().code, /window\.__V = /, "minify: false keeps source formatting");

  // target presets and arrays. The default baseline keeps `?.`; es2015 lowers it.
  build({});
  assert.match(mainChunk().code, /\?\./, "default baseline keeps optional chaining");
  build({ build: { target: ["es2015", "safari10"] } });
  assert.doesNotMatch(mainChunk().code, /\?\./, "array target lowers optional chaining");
  build({ build: { target: "baseline-widely-available" } });
  assert.match(mainChunk().code, /\?\./, "vite preset name accepted");
  // 'modules' (chrome87) lowers `?.` like esbuild does (V8 bug before 91) but keeps `??`.
  build({ build: { target: "modules" } });
  assert.match(mainChunk().code, /\?\?/, "legacy 'modules' preset accepted");
  // ...and via vite.config with the string variants.
  build(`export default { build: { sourcemap: "hidden", minify: "esbuild", target: ["es2015"] } };\n`);
  m = mainChunk();
  assert.ok(fs.existsSync(m.map) && !/sourceMappingURL/.test(m.code), "vite.config hidden sourcemap");
  assert.doesNotMatch(m.code, /\?\./, "vite.config array target");

  // emptyOutDir: inside root, stale files go and .git stays.
  fs.mkdirSync(path.join(app, "dist", ".git"), { recursive: true });
  fs.writeFileSync(path.join(app, "dist", ".git", "HEAD"), "ref");
  fs.writeFileSync(path.join(app, "dist", "stale.txt"), "old");
  build({});
  assert.ok(!fs.existsSync(path.join(app, "dist", "stale.txt")), "stale file removed inside root");
  assert.ok(fs.existsSync(path.join(app, "dist", ".git", "HEAD")), ".git preserved");

  // Outside root: not emptied by default (with a warning), emptied with the flag or config.
  const outside = path.join(base, "outside-dist");
  fs.mkdirSync(outside, { recursive: true });
  fs.writeFileSync(path.join(outside, "stale.txt"), "old");
  let stderr = "";
  try {
    fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
    fs.writeFileSync(path.join(app, "oj.config.json"), JSON.stringify({ build: { outDir: outside } }));
    execSync(`${oj} build ${app}`, { stdio: ["ignore", "pipe", "pipe"] });
  } catch (e) {
    throw new Error("outside build failed: " + e.stderr);
  }
  stderr = execSync(`${oj} build ${app} 2>&1 1>/dev/null`, { shell: "/bin/sh" }).toString();
  assert.ok(fs.existsSync(path.join(outside, "stale.txt")), "outDir outside root is not emptied by default");
  assert.match(stderr, /not inside project root/, "warns about the un-emptied outDir");
  build({ build: { outDir: outside } }, "--emptyOutDir");
  assert.ok(!fs.existsSync(path.join(outside, "stale.txt")), "--emptyOutDir empties it");
  fs.writeFileSync(path.join(outside, "stale.txt"), "old");
  build({ build: { outDir: outside, emptyOutDir: true } });
  assert.ok(!fs.existsSync(path.join(outside, "stale.txt")), "build.emptyOutDir: true empties it");
  assert.ok(fs.existsSync(path.join(outside, "index.html")), "outside build still emitted");

  console.log("BUILD-OPTIONS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("BUILD-OPTIONS E2E FAILED:", err.message);
} finally {
  fs.rmSync(base, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
