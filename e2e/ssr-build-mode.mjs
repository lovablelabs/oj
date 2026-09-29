// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// `oj build --ssr entry --mode staging` must run the plugin hosts of BOTH build
// environments (ssr and client) under mode "staging": Vite resolves one mode per
// build and every plugin's config(env)/configResolved sees it (config.ts
// configEnv.mode). Hard-coding "production" for the SSR build made plugins load
// .env.production branches while oj itself read .env.staging.

import { execSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-ssr-mode-"));
const marks = fs.mkdtempSync(path.join(os.tmpdir(), "oj-ssr-mode-marks-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "ssr-mode", version: "1.0.0" }));
fs.writeFileSync(path.join(app, ".env.production"), "VITE_FLAVOR=prod-flavor\n");
fs.writeFileSync(path.join(app, ".env.staging"), "VITE_FLAVOR=staging-flavor\n");
fs.writeFileSync(
  path.join(app, "src", "entry-server.js"),
  `export const render = () => "<p>" + import.meta.env.MODE + "|" + import.meta.env.VITE_FLAVOR + "</p>";\n`,
);
fs.writeFileSync(
  path.join(app, "src", "entry-client.js"),
  `window.__MODE = import.meta.env.MODE + "|" + import.meta.env.VITE_FLAVOR;\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><div id="root"><!--ssr-outlet--></div><script type="module" src="/src/entry-client.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `import fs from "node:fs";
const log = (line) => fs.appendFileSync(${JSON.stringify(marks)} + "/modes", line + "\\n");
export default [{
  name: "mode-probe",
  config(c, env) { log("config:" + env.mode + ":" + env.command); },
  configResolved(rc) { log("configResolved:" + rc.mode); },
  applyToEnvironment(env) { log("apply:" + env.name + ":" + env.config.mode); return true; },
}];\n`,
);

let failed = false;
try {
  const r = spawnSync(oj, ["build", app, "--ssr", "src/entry-server.js", "--mode", "staging"], { encoding: "utf8" });
  assert.equal(r.status, 0, `ssr build failed:\n${r.stderr}\n${r.stdout}`);
  const lines = fs.readFileSync(path.join(marks, "modes"), "utf8").trim().split("\n");
  const config = lines.filter((l) => l.startsWith("config:"));
  const resolved = lines.filter((l) => l.startsWith("configResolved:"));
  // One plugin host per build environment (ssr, then client), each under the CLI mode.
  assert.ok(config.length >= 2, `config() should run in both the ssr and client hosts:\n${lines.join("\n")}`);
  assert.deepEqual(
    [...new Set(config)],
    ["config:staging:build"],
    `every config(env) sees mode staging:\n${lines.join("\n")}`,
  );
  assert.deepEqual(
    [...new Set(resolved)],
    ["configResolved:staging"],
    `every configResolved sees mode staging:\n${lines.join("\n")}`,
  );
  const applies = lines.filter((l) => l.startsWith("apply:"));
  for (const a of applies) assert.match(a, /:staging$/, `environment config carries the mode: ${a}`);

  const server = fs.readFileSync(path.join(app, "dist", "entry-server.mjs"), "utf8");
  assert.match(server, /staging\|staging-flavor/, "ssr bundle inlines MODE and .env.staging");
  const client = fs
    .readdirSync(path.join(app, "dist", "assets"))
    .filter((f) => f.endsWith(".js"))
    .map((f) => fs.readFileSync(path.join(app, "dist", "assets", f), "utf8"))
    .join("\n");
  assert.match(client, /staging\|staging-flavor/, "client bundle inlines MODE and .env.staging");

  console.log("SSR-BUILD-MODE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("SSR-BUILD-MODE E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
  fs.rmSync(marks, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
