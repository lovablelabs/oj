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

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-config-"));
fs.mkdirSync(path.join(app, ".config"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "cfg-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "vite.config.mjs"), `export default { plugins: [] };\n`);
fs.writeFileSync(
  path.join(app, ".config", "vite.config.mjs"),
  `import base from "../vite.config.mjs";
   export default {
     ...base,
     plugins: [
       ...(base.plugins ?? []),
       { name: "ovr", transformIndexHtml(h) { return h.replace("</head>", '<script>window.__OVR = 1;</script></head>'); } },
     ],
   };\n`,
);
fs.writeFileSync(path.join(app, "src.js"), `window.__READY = true;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src.js"></script></body></html>`,
);

async function htmlWith(args, port) {
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  const srv = spawn(oj, args, { stdio: "ignore" });
  try {
    await waitUp(`http://localhost:${port}/`);
    return await (await fetch(`http://localhost:${port}/`)).text();
  } finally {
    srv.kill("SIGKILL");
    await sleep(300);
  }
}

let failed = false;
try {
  const base = await htmlWith(["dev", app, "--port", "5491"], 5491);
  assert.ok(!/window\.__OVR = 1/.test(base), "root config has no override plugin");

  const overridden = await htmlWith(
    ["dev", app, "--port", "5492", "--config", ".config/vite.config.mjs"],
    5492,
  );
  assert.match(overridden, /window\.__OVR = 1/, "--config loads plugins from the override config");

  // A --config that names a file that does not exist is an error (Vite: "Could
  // not resolve config file"), not a silent fallback to the root config.
  const { spawnSync } = await import("node:child_process");
  const missing = spawnSync(oj, ["build", app, "--config", ".config/nope.config.mjs"], { cwd: app, encoding: "utf8" });
  assert.notEqual(missing.status, 0, "build with a missing --config must fail");
  assert.match(missing.stderr + missing.stdout, /failed to load config from .*nope\.config\.mjs/, `missing --config error:\n${missing.stderr}`);

  console.log("CONFIG-FLAG E2E PASSED");
} catch (err) {
  failed = true;
  console.error("CONFIG-FLAG E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
