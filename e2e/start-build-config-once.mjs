// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// A cold TanStack Start production build evaluates the app's vite.config for
// the build only, as Vite does: the route tree is generated under the build's
// config and env, not a second `serve`/`development` evaluation. Each
// evaluation leaves one extract-cache entry, so a cold build leaves exactly one.

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
  console.log("SKIP start-build-config-once: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const cache = path.join(app, ".oj-cache", "v1");
const extracts = path.join(cache, "config-extract");
// Cold: no extraction and no route tree to restore, so both would evaluate.
for (const dir of ["config-extract", "start-codegen"]) {
  fs.rmSync(path.join(cache, dir), { recursive: true, force: true });
}

execSync(`${oj} build ${app}`, { cwd: repo, stdio: ["ignore", "ignore", "inherit"] });

const entries = fs.existsSync(extracts) ? fs.readdirSync(extracts) : [];
if (entries.length !== 1) {
  throw new Error(`a cold Start build evaluated vite.config ${entries.length} times (want 1): ${entries.join(", ")}`);
}
if (!fs.existsSync(path.join(app, "src", "routeTree.gen.ts"))) {
  throw new Error("the build generated no route tree");
}
console.log("ok start-build-config-once");
