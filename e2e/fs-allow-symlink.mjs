// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Verifies the canonical-at-insert fs allow list keeps today's decisions:
// an allow root reached through a symlink admits both the symlink spelling
// and the real path (pnpm-shape packages), a pnpm-style symlinked
// node_modules package still serves through its import rewrite, an
// allow-listed directory that does not exist at startup starts matching once
// created, and unrelated paths stay 403.

import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";

const OJ = path.join(process.cwd(), "target", "debug", "oj");
const PORT = 5347;
const base = `http://127.0.0.1:${PORT}`;

const workspace = fs.mkdtempSync(path.join(os.tmpdir(), "oj-fslink-"));
const app = path.join(workspace, "app");
const store = path.join(workspace, "store");
const link = path.join(workspace, "sharedlink");
const late = path.join(workspace, "late");

let failed = false;
let child;
try {
  fs.mkdirSync(path.join(app, "src"), { recursive: true });
  fs.mkdirSync(path.join(store, "pkg"), { recursive: true });
  fs.mkdirSync(path.join(store, "mylib"), { recursive: true });
  fs.writeFileSync(path.join(store, "pkg", "util.js"), "export const shared = 42;\n");
  fs.writeFileSync(path.join(workspace, "secret.txt"), "top-secret\n");
  fs.symlinkSync(path.join(store, "pkg"), link);

  // A pnpm-shape dependency: node_modules/mylib is a symlink into the store.
  fs.writeFileSync(path.join(store, "mylib", "package.json"), '{"name":"mylib","main":"index.js"}');
  fs.writeFileSync(path.join(store, "mylib", "index.js"), "export const lib = 'mylib-ok';\n");
  fs.mkdirSync(path.join(app, "node_modules"), { recursive: true });
  fs.symlinkSync(path.join(store, "mylib"), path.join(app, "node_modules", "mylib"));

  fs.writeFileSync(path.join(app, "package.json"), '{"name":"fslink","private":true}');
  fs.writeFileSync(path.join(app, "src", "main.js"), 'import { lib } from "mylib";\nconsole.log(lib);\n');
  fs.writeFileSync(
    path.join(app, "index.html"),
    '<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>',
  );
  fs.writeFileSync(
    path.join(app, "vite.config.mjs"),
    `export default { server: { fs: { allow: [${JSON.stringify(link)}, ${JSON.stringify(late)}] } } };\n`,
  );

  child = spawn(OJ, ["dev", "--port", String(PORT)], { cwd: app, stdio: "ignore" });
  let up = false;
  for (let i = 0; i < 300; i++) {
    try { if ((await fetch(`${base}/`)).ok) { up = true; break; } } catch {}
    await new Promise((r) => setTimeout(r, 100));
  }
  assert.ok(up, "dev server came up");

  const status = async (p) => (await fetch(`${base}/@fs${p}`)).status;

  assert.equal(await status(path.join(link, "util.js")), 200, "symlink spelling denied");
  assert.equal(await status(path.join(store, "pkg", "util.js")), 200, "real path denied");
  console.log("symlinked root:    both spellings -> 200");

  assert.equal(await status(path.join(workspace, "secret.txt")), 403, "unrelated path served");
  console.log("unrelated path:    403");

  // The symlinked package resolves through the import rewrite; its served URL
  // must pass the gate exactly as before.
  const main = await (await fetch(`${base}/src/main.js`)).text();
  const dep = main.match(/["'](\/(?:@fs|@oj-pkg)[^"']+)["']/)?.[1];
  assert.ok(dep, "mylib import was rewritten to a servable URL:\n" + main);
  const depRes = await fetch(`${base}${dep}`);
  assert.equal(depRes.status, 200, `symlinked package module -> ${depRes.status}`);
  assert.match(await depRes.text(), /mylib-ok/, "symlinked package content served");
  console.log("pnpm-style dep:    served via", dep.split("/")[1]);

  // The allow-listed directory did not exist at startup; create it now.
  fs.mkdirSync(late, { recursive: true });
  fs.writeFileSync(path.join(late, "out.js"), "export const late = 1;\n");
  assert.equal(await status(path.join(late, "out.js")), 200, "late-created allow root denied");
  console.log("late-created root: 200 once the directory exists");
  console.log("\nFS ALLOW SYMLINK VERIFIED");
} catch (e) {
  failed = true;
  console.error("FAIL:", e.message || e);
} finally {
  if (child) child.kill("SIGKILL");
  fs.rmSync(workspace, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
