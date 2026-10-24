// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// A cold TanStack Start production build generates the route tree while the
// plugin host boots, not before it: the bundle host leaves the router plugin
// out, so the two never touch the same files, and the bundles wait for both.
// The boot phases order it: the host starts booting before the route-tree
// child exits, and the build still writes a working route tree.

import { execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const app = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");

const installed = fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start"));
if (!installed) {
  console.log("SKIP start-build-codegen-overlap: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

// No cached route tree, so the generator child runs.
fs.rmSync(path.join(app, ".oj-cache", "v1", "start-codegen"), { recursive: true, force: true });

const log = execSync(`${oj} build ${app} 2>&1`, {
  cwd: repo,
  encoding: "utf8",
  env: { ...process.env, OJ_BOOT_PHASES: "1" },
});
const phase = (label) => {
  const m = log.match(new RegExp(`\\[oj-phase\\] (\\d+) ${label}`));
  if (!m) throw new Error(`no "${label}" phase in the build log:\n${log.slice(-1500)}`);
  return Number(m[1]);
};
const booting = phase("start build: plugin host booting");
const generated = phase("route tree generation: child exited");
if (booting > generated) {
  throw new Error(
    `the plugin host started booting only after the route tree was generated (${booting} > ${generated})`,
  );
}
if (!fs.existsSync(path.join(app, "src", "routeTree.gen.ts"))) {
  throw new Error("the build generated no route tree");
}
console.log("ok start-build-codegen-overlap");
