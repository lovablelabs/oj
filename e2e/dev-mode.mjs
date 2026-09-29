// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// `oj dev --mode <mode>` must select `.env.<mode>`, set `import.meta.env.MODE`,
// and hand the mode to a function config, like `vite dev --mode`. Without the
// flag the mode is `development`.

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
const PORT = 5490;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-devmode-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "devmode-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, ".env.development"), "VITE_FLAVOR=dev-flavor\n");
fs.writeFileSync(path.join(app, ".env.staging"), "VITE_FLAVOR=staging-flavor\n");
fs.writeFileSync(
  path.join(app, "oj.config.js"),
  `export default ({ mode }) => ({ define: { __CFG_MODE__: JSON.stringify(mode) } });\n`,
);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `window.__MODE = import.meta.env.MODE; window.__FLAVOR = import.meta.env.VITE_FLAVOR;\n` +
    `window.__DEV = import.meta.env.DEV; window.__CFG = __CFG_MODE__;\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

async function served(args) {
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  const srv = spawn(oj, ["dev", app, "--port", String(PORT), ...args], { stdio: "ignore" });
  try {
    await waitUp(`http://localhost:${PORT}/`);
    return await (await fetch(`http://localhost:${PORT}/src/main.js`)).text();
  } finally {
    srv.kill("SIGKILL");
    await sleep(300);
  }
}

let failed = false;
try {
  const dev = await served([]);
  assert.match(dev, /__MODE = "development"/, `default mode is development:\n${dev}`);
  assert.match(dev, /"dev-flavor"/, "default loads .env.development");
  assert.match(dev, /__CFG = "development"/, "config fn sees mode=development");

  const staging = await served(["--mode", "staging"]);
  assert.match(staging, /__MODE = "staging"/, `--mode sets MODE:\n${staging}`);
  assert.match(staging, /"staging-flavor"/, "--mode staging loads .env.staging");
  assert.doesNotMatch(staging, /dev-flavor/, ".env.development is not loaded for --mode staging");
  assert.match(staging, /__DEV = true/, "a non-production dev mode is still DEV");
  assert.match(staging, /__CFG = "staging"/, "config fn sees mode=staging");
  console.log("DEV-MODE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("DEV-MODE E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
