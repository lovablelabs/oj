// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// A Start dependency entered through its ESM `module` entry imports a bare
// sibling whose `main` is a UMD wrapper (no `exports` map — the
// detect-gpu/@react-three/drei shape): the host must resolve that bare import
// with the same Vite-style resolver that picked the importer's entry, or the
// engine strict-links the ESM file against the UMD, whose named exports the
// CJS lexer cannot see, and SSR dies with "does not provide an export named".
// Run with a built target/debug/oj; installs a copy of the start-app fixture
// (skips when offline).
import { execSync, spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const fixture = path.join(repo, "e2e", "fixtures", "start-app");

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-dep-esm-bare-"));
const cleanup = () => fs.rmSync(app, { recursive: true, force: true });
fs.cpSync(fixture, app, {
  recursive: true,
  filter: (src) => !/\/(node_modules|\.oj-cache|dist)(\/|$)/.test(src),
});
try {
  execSync("npm install --no-audit --no-fund --no-package-lock --loglevel=error", { cwd: app, stdio: "ignore" });
} catch {
  console.log("SKIP dep-esm-bare-import: could not install the fixture (offline?)");
  cleanup();
  process.exit(0);
}

// ui-lib: dual-entry, no `exports`, no `type` — the resolver picks `module`
// (ESM), which plain Node would never load.
const uiLib = path.join(app, "node_modules", "ui-lib");
fs.mkdirSync(uiLib, { recursive: true });
fs.writeFileSync(
  path.join(uiLib, "package.json"),
  JSON.stringify({ name: "ui-lib", version: "1.0.0", main: "index.cjs.js", module: "index.esm.js" }),
);
fs.writeFileSync(path.join(uiLib, "index.esm.js"), `import { getTier } from "gpu-lib";\nexport const tier = getTier();\n`);
fs.writeFileSync(path.join(uiLib, "index.cjs.js"), `const { getTier } = require("gpu-lib");\nexports.tier = getTier();\n`);

// gpu-lib: `main` is a MINIFIED UMD wrapper (detect-gpu's shape) — the
// factory receives `exports` under a renamed parameter, so no CJS export
// lexer can see its names; `module` is honest ESM.
const gpuLib = path.join(app, "node_modules", "gpu-lib");
fs.mkdirSync(gpuLib, { recursive: true });
fs.writeFileSync(
  path.join(gpuLib, "package.json"),
  JSON.stringify({ name: "gpu-lib", version: "1.0.0", main: "dist.umd.js", module: "dist.esm.js" }),
);
fs.writeFileSync(
  path.join(gpuLib, "dist.umd.js"),
  `!function(e,t){"object"==typeof exports&&"undefined"!=typeof module?t(exports):"function"==typeof define&&define.amd?define(["exports"],t):t((e="undefined"!=typeof globalThis?globalThis:e||self).GpuLib={})}(this,(function(e){"use strict";e.getTier=function(){return 3}}));
`,
);
fs.writeFileSync(path.join(gpuLib, "dist.esm.js"), `export function getTier() { return 3; }\n`);

// Reach ui-lib from a route so SSR links the chain (the fixture's route
// tree is code-based, so the new route registers in routeTree.ts).
fs.writeFileSync(
  path.join(app, "src", "routes", "gpu-tier.tsx"),
  `import { createRoute } from "@tanstack/react-router";
import { tier } from "ui-lib";

import { rootRoute } from "./__root";

export const gpuTierRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/gpu-tier",
  component: () => <main data-testid="gpu-tier">{"tier:" + tier}</main>,
});
`,
);
const routeTreePath = path.join(app, "src", "routeTree.ts");
const routeTree = fs
  .readFileSync(routeTreePath, "utf8")
  .replace('import { rootRoute }', 'import { gpuTierRoute } from "./routes/gpu-tier";\nimport { rootRoute }')
  .replace("requestUrlRoute]", "requestUrlRoute, gpuTierRoute]");
if (!routeTree.includes("gpuTierRoute]")) throw new Error("could not register the test route");
fs.writeFileSync(routeTreePath, routeTree);

const port = 5217;
const server = spawn(oj, ["dev", ".", "--port", String(port), "--host=127.0.0.1"], {
  cwd: app,
  stdio: ["ignore", "pipe", "pipe"],
});
let log = "";
server.stdout.on("data", (d) => (log += d));
server.stderr.on("data", (d) => (log += d));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
try {
  let body;
  let status = 0;
  for (let i = 0; i < 120; i++) {
    try {
      const res = await fetch(`http://127.0.0.1:${port}/gpu-tier`);
      status = res.status;
      body = await res.text();
      if (status === 200 || status === 500) break;
    } catch {}
    await sleep(500);
  }
  if (status !== 200 || !body.includes("tier:3")) {
    console.error(log.slice(-4000));
    throw new Error(
      `SSR of an ESM dependency's bare import failed: status ${status}` +
        (log.includes("does not provide an export") ? " (strict-linked the UMD main)" : ""),
    );
  }
} finally {
  server.kill("SIGKILL");
  cleanup();
}
console.log("dep-esm-bare-import: ok");
