// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Verifies the dep optimizer skips linked / workspace packages (symlinked into
// node_modules): pre-bundling them freezes their source so edits stop HMR-ing.
// A normal node_modules dep IS pre-bundled (in the manifest, served via
// /@oj-deps); a symlinked-to-outside-source dep is NOT (served on demand).

import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const esbuildSrc = path.join(repo, "e2e/fixtures/start-app/node_modules/esbuild");
const port = 5335;

if (!fs.existsSync(esbuildSrc)) {
  console.log("SKIP optimize-linked: esbuild fixture not installed");
  process.exit(0);
}

const ws = fs.mkdtempSync(path.join(os.tmpdir(), "oj-linked-"));
const app = path.join(ws, "app");
const nm = path.join(app, "node_modules");
const linkSrc = path.join(ws, "packages", "link-lib"); // workspace source, OUTSIDE node_modules

let server;
let failed = false;
try {
  fs.mkdirSync(path.join(nm, "norm-lib"), { recursive: true });
  fs.mkdirSync(linkSrc, { recursive: true });
  fs.symlinkSync(esbuildSrc, path.join(nm, "esbuild"));
  const esbuildScoped = path.join(repo, "e2e/fixtures/start-app/node_modules/@esbuild");
  if (fs.existsSync(esbuildScoped)) fs.symlinkSync(esbuildScoped, path.join(nm, "@esbuild"));

  // Normal node_modules dep (real dir) -> should be pre-bundled.
  fs.writeFileSync(
    path.join(nm, "norm-lib", "package.json"),
    '{"name":"norm-lib","version":"1.0.0","main":"index.js"}',
  );
  fs.writeFileSync(path.join(nm, "norm-lib", "index.js"), 'module.exports = { hi: function () { return "norm"; } };\n');
  // Workspace package symlinked into node_modules -> should be skipped.
  fs.writeFileSync(path.join(linkSrc, "package.json"), '{"name":"link-lib","version":"1.0.0","module":"index.js"}');
  fs.writeFileSync(path.join(linkSrc, "index.js"), 'export const hi = () => "link";\n');
  fs.symlinkSync(linkSrc, path.join(nm, "link-lib"), "dir");

  fs.writeFileSync(path.join(app, "package.json"), '{"name":"linkedapp","private":true}');
  fs.writeFileSync(
    path.join(app, "index.html"),
    '<!doctype html><html><head></head><body><script type="module" src="/main.js"></script></body></html>',
  );
  fs.writeFileSync(
    path.join(app, "main.js"),
    'import { hi as h1 } from "norm-lib";\nimport { hi as h2 } from "link-lib";\nwindow.__R = h1() + h2();\n',
  );

  server = spawn(oj, ["dev", app, "--port", String(port)], {
    stdio: "ignore",
    env: { ...process.env, OJ_OPTIMIZE_SCAN: "1" },
  });
  await waitUp(`http://localhost:${port}/`);
  // let the optimizer settle
  await new Promise((r) => setTimeout(r, 800));

  const manifestPath = path.join(app, ".oj-cache", "v1", "deps", "manifest.json");
  if (!fs.existsSync(manifestPath)) throw new Error("no deps manifest emitted");
  const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
  const meta = manifest.metadata || {};
  if (!("norm-lib" in meta)) throw new Error(`normal dep should be pre-bundled; manifest: ${JSON.stringify(meta)}`);
  if ("link-lib" in meta)
    throw new Error(`linked/workspace dep must NOT be pre-bundled; manifest: ${JSON.stringify(meta)}`);
  console.log("norm-lib pre-bundled:  yes");
  console.log("link-lib skipped:      yes (served as workspace source)");
  console.log("\nLINKED-DEP SKIP VERIFIED");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  if (server) server.kill("SIGKILL");
  fs.rmSync(ws, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
