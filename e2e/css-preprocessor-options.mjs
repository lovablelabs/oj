// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// css.preprocessorOptions for Sass in dev and build: `additionalData` is
// prepended, `loadPaths` resolve bare `@use`, and node_modules packages resolve
// through their package.json `sass`/`style` entry (also with the `~` prefix),
// as Vite's sass importer does.

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
const PORT = 5541;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-sassopts-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.mkdirSync(path.join(app, "styles"), { recursive: true });
fs.mkdirSync(path.join(app, "node_modules", "@acme", "tokens", "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "sassopts-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "vite.config.js"),
  `export default { css: { preprocessorOptions: { scss: { additionalData: "$brand: #f00;", loadPaths: ["styles"] } } } };\n`,
);
fs.writeFileSync(path.join(app, "styles", "_theme.scss"), `$accent: #abcdef;\n`);
fs.writeFileSync(
  path.join(app, "node_modules", "@acme", "tokens", "package.json"),
  JSON.stringify({ name: "@acme/tokens", version: "1.0.0", main: "index.js", sass: "src/index.scss" }),
);
fs.writeFileSync(path.join(app, "node_modules", "@acme", "tokens", "src", "index.scss"), `$space: 7px;\n`);
fs.writeFileSync(path.join(app, "node_modules", "@acme", "tokens", "src", "_mixins.scss"), `@mixin pad { padding: 9px; }\n`);
fs.writeFileSync(
  path.join(app, "src", "a.scss"),
  `@use "theme";\n@use "@acme/tokens" as t;\n@use "~@acme/tokens/src/mixins";\n` +
    `.a { color: $brand; border-color: theme.$accent; margin: t.$space; @include mixins.pad; }\n`,
);
fs.writeFileSync(path.join(app, "src", "main.js"), `import "./a.scss";\nwindow.__OK = true;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

function expectCompiled(css, label) {
  assert.match(css, /color:\s*(#f00|red)/, `${label}: additionalData variable`);
  assert.match(css, /#abcdef/, `${label}: loadPaths @use`);
  assert.match(css, /margin:\s*7px/, `${label}: package.json sass entry`);
  assert.match(css, /padding:\s*9px/, `${label}: ~pkg/path import`);
  assert.doesNotMatch(css, /\$brand|\$accent|@use/, `${label}: sass left in output`);
}

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${PORT}/`);
  const direct = await (await fetch(`http://localhost:${PORT}/src/a.scss?direct`)).text();
  expectCompiled(direct, "dev");
  console.log("[dev] preprocessorOptions + node_modules sass resolution OK");
  srv.kill("SIGKILL");
  await sleep(300);

  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const assets = path.join(app, "dist", "assets");
  const css = fs.readdirSync(assets).filter((f) => f.endsWith(".css")).map((f) => fs.readFileSync(path.join(assets, f), "utf8")).join("\n");
  expectCompiled(css, "build");
  console.log("[build] preprocessorOptions + node_modules sass resolution OK");
  console.log("CSS-PREPROCESSOR-OPTIONS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("CSS-PREPROCESSOR-OPTIONS E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
