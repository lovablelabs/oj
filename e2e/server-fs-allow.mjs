// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Verifies server.fs.allow: a file outside the project root (e.g. a monorepo
// shared package) is denied over /@fs unless the config allow-lists its dir.
// Without server.fs, oj only serves import-reached files — so a bare /@fs dial
// to an outside path 403s; with server.fs.allow it is served.

import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { waitUp } from "./util.mjs";

const OJ = path.join(process.cwd(), "target", "debug", "oj");

const workspace = fs.mkdtempSync(path.join(os.tmpdir(), "oj-fsallow-"));
const app = path.join(workspace, "app");
const shared = path.join(workspace, "shared");
const sharedFile = path.join(shared, "util.js");

// A SIGKILLed server's node children flush for a beat and race the removal
// (ENOTEMPTY); retry like start-cloudflare-dev.mjs / the proxy tests do.
const rmDirRetry = (dir) => {
  for (let i = 0; ; i++) {
    try {
      return fs.rmSync(dir, { recursive: true, force: true });
    } catch (e) {
      if (i >= 20) return;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100);
    }
  }
};

let failed = false;
let child;
const startApp = async (port, viteConfig) => {
  rmDirRetry(app);
  fs.mkdirSync(path.join(app, "src"), { recursive: true });
  fs.writeFileSync(path.join(app, "package.json"), '{"name":"fsallow","private":true}');
  fs.writeFileSync(
    path.join(app, "index.html"),
    '<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>',
  );
  fs.writeFileSync(path.join(app, "src", "main.js"), "export const v = 1;\n");
  if (viteConfig) fs.writeFileSync(path.join(app, "vite.config.mjs"), viteConfig);
  const c = spawn(OJ, ["dev", "--port", String(port)], { cwd: app, stdio: "ignore" });
  try {
    await waitUp(`http://localhost:${port}/`, { proc: c });
    return c;
  } catch (e) {
    c.kill("SIGKILL");
    throw e;
  }
};

try {
  fs.mkdirSync(shared, { recursive: true });
  fs.writeFileSync(sharedFile, "export const shared = 42;\n");
  const fsUrl = (port) => `http://localhost:${port}/@fs${sharedFile}`;

  // Case A: no server.fs -> outside file denied.
  child = await startApp(5333, null);
  const denied = await fetch(fsUrl(5333));
  if (denied.status !== 403) throw new Error(`outside /@fs should be 403 without allow, got ${denied.status}`);
  child.kill("SIGKILL");
  child = null;
  console.log("without allow:  /@fs outside root -> 403");

  // Case B: server.fs.allow the shared dir -> served.
  const cfg = `export default { server: { fs: { allow: [${JSON.stringify(shared)}] } } };\n`;
  child = await startApp(5334, cfg);
  const ok = await fetch(fsUrl(5334));
  if (ok.status !== 200) throw new Error(`allow-listed /@fs should be 200, got ${ok.status}`);
  const body = await ok.text();
  if (!body.includes("42")) throw new Error(`served /@fs file missing its content:\n${body}`);
  console.log("with allow:     /@fs outside root -> 200");
  child.kill("SIGKILL");
  child = null;

  // Case C: server.fs.strict: false -> Vite's isFileLoadingAllowed skips the
  // allow list entirely, so the outside file is served; the deny list still
  // applies (a `.env` next to it stays 403).
  fs.writeFileSync(path.join(shared, ".env"), "SECRET=1\n");
  child = await startApp(5335, "export default { server: { fs: { strict: false } } };\n");
  const lax = await fetch(fsUrl(5335));
  if (lax.status !== 200) throw new Error(`strict:false should serve /@fs outside root, got ${lax.status}`);
  const stillDenied = await fetch(`http://localhost:5335/@fs${path.join(shared, ".env")}`);
  if (stillDenied.status !== 403) throw new Error(`strict:false must keep fs.deny, got ${stillDenied.status}`);
  child.kill("SIGKILL");
  child = null;
  console.log("strict:false:   /@fs outside root -> 200, denied .env -> 403");

  // Case D: the default allow list is Vite's searchForWorkspaceRoot, where a
  // `.git` directory is NOT a workspace marker: a project nested inside a
  // repository must not expose the repository over /@fs.
  fs.mkdirSync(path.join(workspace, ".git"), { recursive: true });
  fs.writeFileSync(path.join(workspace, "package.json"), '{"name":"repo"}');
  child = await startApp(5336, null);
  const repoFile = await fetch(fsUrl(5336));
  if (repoFile.status !== 403) throw new Error(`.git parent must not widen fs.allow, got ${repoFile.status}`);
  child.kill("SIGKILL");
  child = null;
  console.log("nested in repo: /@fs sibling dir -> 403 (.git is not a workspace marker)");

  // Case E: a real workspace marker (pnpm-workspace.yaml) does widen it.
  fs.writeFileSync(path.join(workspace, "pnpm-workspace.yaml"), "packages:\n  - app\n");
  child = await startApp(5337, null);
  const wsFile = await fetch(fsUrl(5337));
  if (wsFile.status !== 200) throw new Error(`pnpm workspace root should be allowed, got ${wsFile.status}`);
  console.log("pnpm workspace: /@fs sibling dir -> 200");
  console.log("\nserver.fs.allow VERIFIED");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message);
} finally {
  if (child) child.kill("SIGKILL");
  rmDirRetry(workspace);
}
process.exit(failed ? 1 : 0);
