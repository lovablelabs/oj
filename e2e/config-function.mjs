// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

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

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-fncfg-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "fncfg-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "oj.config.js"),
  `export default ({ command, mode }) => ({\n` +
    `  base: command === "build" ? "/prod/" : "/dev/",\n` +
    `  define: { __MODE__: JSON.stringify(mode) },\n` +
    `});\n`,
);
fs.writeFileSync(path.join(app, "src", "main.js"), `window.__M = __MODE__;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

let failed = false;
try {
  // dev: command=serve, mode=development
  const srv = spawn(oj, ["dev", app, "--port", "5375"], { stdio: "ignore" });
  await waitUp("http://localhost:5375/dev/");
  const devMain = await (await fetch("http://localhost:5375/dev/src/main.js")).text();
  srv.kill("SIGKILL");
  await sleep(200);
  assert.match(devMain, /development/, "dev config define did not apply mode=development");

  // build: command=build, mode=production
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const html = fs.readFileSync(path.join(app, "dist", "index.html"), "utf8");
  assert.match(html, /src="\/prod\//, "build config did not apply base=/prod/");
  const built = fs.readdirSync(path.join(app, "dist", "assets")).find((f) => f.startsWith("main-"));
  const buildMain = fs.readFileSync(path.join(app, "dist", "assets", built), "utf8");
  assert.match(buildMain, /production/, "build config define did not apply mode=production");

  console.log("CONFIG-FUNCTION E2E PASSED");
} catch (err) {
  failed = true;
  console.error("CONFIG-FUNCTION E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
