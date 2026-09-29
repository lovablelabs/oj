// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// A TanStack Start production build honors the build options Vite applies to
// any app: `--out` / `build.outDir`, `base`, `build.sourcemap`, `build.minify`.
// The fixture is built with a config override (its own config plus a sub-path
// base, source maps and no minification) into a temporary outDir, then served
// to check the base-prefixed asset URLs resolve.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const app = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");

const installed =
  fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(app, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP start-build-options: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-build-options-"));
const out = path.join(tmp, "out");
const config = path.join(tmp, "vite.config.ts");
// The override config imports the fixture's own config (and its plugins), so it
// needs the fixture's dependencies resolvable from where it lives.
fs.symlinkSync(path.join(app, "node_modules"), path.join(tmp, "node_modules"));
fs.writeFileSync(path.join(tmp, "package.json"), '{"type":"module"}\n');
fs.writeFileSync(
  config,
  `import base from ${JSON.stringify(path.join(app, "vite.config"))};\n` +
    `export default { ...base, base: "/app/", build: { ...(base as any).build, sourcemap: true, minify: false } };\n`,
);

// A dist/ left by an earlier fixture build would read as "--out ignored" below;
// the assertion is about this build writing there, so start from a clean slate.
fs.rmSync(path.join(app, "dist"), { recursive: true, force: true });

try {
  const stderr = execSync(`${oj} build ${app} --config ${config} --out ${out} 2>&1 1>/dev/null`, {
    cwd: repo,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "inherit"],
  });
  if (/failed to load config/.test(stderr)) {
    throw new Error("the override config did not load for the plugin containers:\n" + stderr.slice(-800));
  }
  if (fs.existsSync(path.join(app, "dist"))) {
    throw new Error("--out was ignored: the fixture's own dist/ was written");
  }
  for (const f of ["server.mjs", "server-bundle.mjs", "server-bundle.mjs.map", "client"]) {
    if (!fs.existsSync(path.join(out, f))) throw new Error(`outDir is missing ${f}`);
  }
  const assets = path.join(out, "client", "assets");
  const clientJs = fs.readdirSync(assets).find((f) => /^client-.*\.js$/.test(f));
  if (!clientJs) throw new Error("no client entry chunk in outDir");
  if (!fs.existsSync(path.join(assets, clientJs + ".map")))
    throw new Error("build.sourcemap: true emitted no client .map");
  const code = fs.readFileSync(path.join(assets, clientJs), "utf8");
  if (code.split("\n").length < 50) throw new Error("build.minify: false was ignored (client chunk is minified)");
  if (!code.includes("sourceMappingURL=")) throw new Error("client chunk has no sourceMappingURL comment");
  if (!fs.readFileSync(path.join(out, "server.mjs"), "utf8").includes('const BASE = "/app/"')) {
    throw new Error("server.mjs does not know the base");
  }

  const port = 6510;
  const srv = spawn("node", [path.join(out, "server.mjs")], {
    cwd: out,
    stdio: "ignore",
    env: { ...process.env, PORT: String(port) },
  });
  try {
    await waitUp(`http://localhost:${port}/`);
    const home = await fetch(`http://localhost:${port}/`);
    const html = await home.text();
    if (home.status !== 200) throw new Error(`/ returned ${home.status}`);
    if (!html.includes(`src="/app/assets/${clientJs}"`)) {
      throw new Error("the rendered page does not load the client entry under the base:\n" + html.slice(0, 800));
    }
    if (/(?:src|href)="\/assets\//.test(html)) throw new Error("an asset URL was emitted without the base prefix");
    const asset = await fetch(`http://localhost:${port}/app/assets/${clientJs}`);
    if (asset.status !== 200) throw new Error(`base-prefixed asset returned ${asset.status}`);
  } finally {
    srv.kill("SIGKILL");
  }
  console.log("start-build-options: outDir, base, sourcemap and minify honored");
} finally {
  fs.rmSync(tmp, { recursive: true, force: true });
}
