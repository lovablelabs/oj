// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// TanStack Start production build: Vite's `ssr.external` keeps the listed
// dependencies out of the server bundle (bare imports resolved from
// node_modules at run time), and the built server still renders with them
// external. Without the option the bundle stays self-contained.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const app = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 3096);

const installed =
  fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(app, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP start ssr.external: fixture deps not installed");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const rm = (p) => {
  for (let i = 0; ; i++) {
    try {
      return fs.rmSync(p, { recursive: true, force: true });
    } catch (e) {
      if (i >= 20) throw e;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100);
    }
  }
};
const must = (cond, msg) => { if (!cond) throw new Error(msg); };
const dist = path.join(app, "dist");

function build(args) {
  rm(dist);
  rm(path.join(app, ".oj-cache"));
  execSync(`${oj} build ${args} ${app}`, { cwd: repo, stdio: ["ignore", "ignore", "inherit"] });
  return fs.readFileSync(path.join(dist, "server-bundle.mjs"), "utf8");
}
const bareImports = (code, pkg) => code.match(new RegExp(`from\\s*["']${pkg}(?:/[^"']*)?["']`, "g")) ?? [];

try {
  const plain = build("");
  must(bareImports(plain, "react").length === 0 && bareImports(plain, "react-dom").length === 0,
    "default: the Start server bundle should stay self-contained (react inlined)");

  const external = build("--config vite.ssr-external.config.ts");
  must(bareImports(external, "react").length > 0, "ssr.external: react is not a bare import of the server bundle");
  must(bareImports(external, "react-dom").length > 0, "ssr.external: react-dom is not a bare import of the server bundle");
  must(!/react-dom\/cjs\/react-dom-server/.test(external), "ssr.external: react-dom's internals were still inlined");
  const size = (s) => Buffer.byteLength(s);
  must(size(external) < size(plain), `ssr.external: bundle did not shrink (${size(external)} vs ${size(plain)})`);

  const srv = spawn("node", [path.join(dist, "server.mjs")], {
    cwd: app, stdio: "ignore", env: { ...process.env, PORT: String(PORT) },
  });
  try {
    const up = await waitUp(`http://localhost:${PORT}/`).then(() => true, () => false);
    must(up, `built server on :${PORT} did not start with react external`);
    const html = await (await fetch(`http://localhost:${PORT}/`)).text();
    for (const marker of ["HOME!", "server-fn-marker", "fixture-define-marker", "Alpha Widget, Beta Widget"]) {
      must(html.includes(marker), `ssr.external: built server did not render "${marker}"`);
    }
    console.log("start-prod: ssr.external keeps react/react-dom bare; server renders with them external");
  } finally {
    srv.kill("SIGKILL");
  }
  console.log("\nSTART SSR.EXTERNAL PASSED");
} catch (e) {
  console.error("\nSTART SSR.EXTERNAL FAILED:", e.message);
  process.exit(1);
} finally {
  rm(dist);
}
