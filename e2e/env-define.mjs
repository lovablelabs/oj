// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Client import.meta.env parity with Vite: values come from .env files,
// the actual process environment (which wins over files), and plugin
// config() hook env mutations — not from .env files alone.

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

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-env-define-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "env-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, ".env.development"),
  "VITE_FROM_DOTENV=dotenv-value\nVITE_OVERRIDDEN=file-value\nUNPREFIXED_SECRET=leak-me-not\n",
);
fs.writeFileSync(
  path.join(app, "vite.config.mjs"),
  `export default {
     plugins: [
       {
         name: "env-mutator",
         config() { process.env.VITE_FROM_PLUGIN = "plugin-value"; },
       },
     ],
   };\n`,
);
fs.writeFileSync(
  path.join(app, "probe.js"),
  `export const probe = {
     dotenv: import.meta.env.VITE_FROM_DOTENV,
     shell: import.meta.env.VITE_FROM_SHELL,
     plugin: import.meta.env.VITE_FROM_PLUGIN,
     overridden: import.meta.env.VITE_OVERRIDDEN,
     secret: import.meta.env.UNPREFIXED_SECRET,
   };\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/probe.js"></script></body></html>`,
);

const port = 5241;
const srv = spawn(oj, ["dev", "--port", String(port)], {
  cwd: app,
  stdio: "ignore",
  env: {
    ...process.env,
    VITE_FROM_SHELL: "shell-value",
    VITE_OVERRIDDEN: "shell-wins",
  },
});
try {
  let served = null;
  await waitUp(`http://localhost:${port}/probe.js`, {
    until: async (res) => res.ok && (served = await res.text()) !== undefined,
  }).catch(() => {});
  assert.ok(served, "dev server never served /probe.js");
  assert.match(served, /dotenv-value/, ".env file var must inline");
  assert.match(served, /shell-value/, "process-env VITE_* must inline");
  assert.match(served, /plugin-value/, "plugin config() env mutation must inline");
  assert.match(served, /shell-wins/, "process env must win over the .env file value");
  assert.doesNotMatch(served, /file-value/, "overridden file value must not survive");
  assert.doesNotMatch(served, /leak-me-not/, "unprefixed vars must not reach the client");
  console.log("env-define e2e: ok");
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
