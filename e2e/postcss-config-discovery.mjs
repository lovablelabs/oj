// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// PostCSS config discovery like postcss-load-config: a `postcss.config.mjs` at
// the WORKSPACE root (not the package) applies to a package in `packages/web`,
// with `postcss` resolved from the root's node_modules, and it runs on Sass
// output, in dev and in the build. Uses the postcss install from a sibling
// checkout when one exists; skips otherwise.

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
const PORT = 5544;

const postcssSources = [
  path.join(repo, "e2e", "node_modules"),
  path.join(repo, "playground", "node_modules"),
  path.join(repo, "..", "twenty-oj", "node_modules"),
  path.join(repo, "..", "..", "twenty-oj", "node_modules"),
];
const nodeModules = postcssSources.find((d) => fs.existsSync(path.join(d, "postcss", "package.json")));
if (!nodeModules) {
  console.log("POSTCSS-CONFIG-DISCOVERY E2E SKIPPED (no postcss install found)");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const ws = fs.mkdtempSync(path.join(os.tmpdir(), "oj-postcss-ws-"));
const app = path.join(ws, "packages", "web");
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.mkdirSync(path.join(ws, ".git"));
fs.symlinkSync(nodeModules, path.join(ws, "node_modules"), "dir");
fs.writeFileSync(path.join(ws, "package.json"), JSON.stringify({ name: "ws", private: true, workspaces: ["packages/*"] }));
fs.writeFileSync(
  path.join(ws, "postcss.config.mjs"),
  `export default { plugins: [{ postcssPlugin: "oj-marker", Once(root) { root.append(".from-postcss { color: green; }"); } }] };\n`,
);
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "web", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "a.scss"), `$c: red;\n.a { color: $c; }\n`);
fs.writeFileSync(path.join(app, "src", "main.js"), `import "./a.scss";\nwindow.__OK = true;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${PORT}/`);
  const css = await (await fetch(`http://localhost:${PORT}/src/a.scss?direct`)).text();
  assert.match(css, /color:\s*red/, `sass compiled:\n${css}`);
  assert.match(css, /\.from-postcss/, `workspace-root postcss config applied after sass (dev):\n${css}`);
  console.log("[dev] workspace postcss.config.mjs applied on sass output OK");
  srv.kill("SIGKILL");
  await sleep(300);

  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const assets = path.join(app, "dist", "assets");
  const built = fs.readdirSync(assets).filter((f) => f.endsWith(".css")).map((f) => fs.readFileSync(path.join(assets, f), "utf8")).join("\n");
  assert.match(built, /\.from-postcss/, `postcss applied in build:\n${built}`);
  assert.match(built, /color:\s*red/, "sass compiled in build");
  console.log("[build] workspace postcss.config.mjs applied OK");
  console.log("POSTCSS-CONFIG-DISCOVERY E2E PASSED");
} catch (err) {
  failed = true;
  console.error("POSTCSS-CONFIG-DISCOVERY E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(ws, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
