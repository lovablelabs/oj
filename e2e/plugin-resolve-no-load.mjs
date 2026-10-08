// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

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

// A resolver plugin maps a bare specifier to a real file the on-disk resolver
// would never reach (outside the project tree), and has NO load hook. Rollup's
// contract: a resolveId result naming a real file IS the module, and a load hook
// returning nothing means read it from disk. oj used to 404 that module.
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-resolve-noload-"));
const ext = fs.mkdtempSync(path.join(os.tmpdir(), "oj-ext-dep-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "rnl-app", version: "1.0.0" }));
// The resolved file lives outside the project root, so only the plugin can reach it.
fs.writeFileSync(path.join(ext, "external-dep.js"), `export const MARK = "external-dep-served-ok";\n`);
// The plugin resolves THROUGH a symlink (a linked workspace package's shape;
// macOS tmpdirs are symlinked /var paths anyway): the allow-list entry is made
// from the raw resolved path while the serving gate checks the request's
// canonical path, and a raw-only entry 403'd the module.
const extLink = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "oj-ext-link-")), "pkg");
fs.symlinkSync(ext, extLink);
fs.writeFileSync(path.join(app, "src", "entry.js"), `import { MARK } from "my-external-dep";\nwindow.__mark = MARK;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/entry.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `import path from "node:path";
   const target = ${JSON.stringify(path.join(extLink, "external-dep.js"))};
   export default [{
     name: "external-resolver",
     enforce: "pre",
     // Resolution is the whole job — no load hook.
     resolveId(id) { return id === "my-external-dep" ? target : null; },
   }];\n`,
);

const port = 5493;
const base = `http://127.0.0.1:${port}`;
async function reachable() {
  return waitUp(`${base}/`, { init: () => ({ signal: AbortSignal.timeout(1500) }) }).then(
    () => true,
    () => false,
  );
}

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
try {
  assert.ok(await reachable(), "dev server came up");

  const entry = await (await fetch(`${base}/src/entry.js`)).text();
  const idUrl = entry.match(/\/@id\/[a-f0-9]+\?importer=[a-f0-9]+/)?.[0];
  assert.ok(idUrl, "the bare specifier was rewritten to a /@id/ URL:\n" + entry);

  const res = await fetch(`${base}${idUrl}`); // fetch follows the redirect
  assert.equal(res.status, 200, `the resolved-but-unloaded module must be served, got ${res.status}`);
  const mod = await res.text();
  assert.match(mod, /external-dep-served-ok/, "the resolved file's bytes were served from disk:\n" + mod);

  console.log("PLUGIN-RESOLVE-NO-LOAD E2E PASSED");
} catch (e) {
  failed = true;
  console.error("PLUGIN-RESOLVE-NO-LOAD E2E FAILED:", e.message || e);
} finally {
  srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
  fs.rmSync(ext, { recursive: true, force: true });
  fs.rmSync(path.dirname(extLink), { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
