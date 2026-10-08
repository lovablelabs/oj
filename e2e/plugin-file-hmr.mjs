// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// A plugin resolveId that answers a bare specifier with a plain app file (a
// framework's generated file, an alias) must leave the file on the normal
// module pipeline: served at its own URL, watched, in the module graph. The
// plugin route used to compile such ids itself, so the file was never watched
// and editing it did nothing. The plugin's `load` keeps its say: the normal
// pipeline asks it before the fs read.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const PORT = 5546;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

// realpath: plugins build paths from the resolved config.root, which is
// canonical (macOS tmpdirs are symlinks).
const app = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "oj-pluginfile-")));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
const w = (rel, s) => fs.writeFileSync(path.join(app, rel), s);
w("package.json", JSON.stringify({ name: "pluginfile-app", version: "1.0.0" }));
w("src/tree.gen.js", `export const label = "tree-1";\n`);
w("src/main.js", `import { label } from "routes:tree";\nwindow.__LABEL = label;\nwindow.__READY = true;\n`);
w(
  "index.html",
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);
// resolveId answers the bare specifier with a real file; load returns content
// for it too, which must not pull the file off the normal pipeline (Vite asks
// load for every real file as well).
w(
  "oj.plugins.mjs",
  `import { readFileSync } from "node:fs";
   const TREE = ${JSON.stringify(path.join(app, "src", "tree.gen.js"))};
   export default [{
     name: "routes-gen",
     resolveId(id) {
       if (id === "routes:tree") return TREE;
     },
     load(id) {
       if (id === TREE) return readFileSync(TREE, "utf8");
     },
   }];\n`,
);

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
let browser;
try {
  await waitUp(`http://localhost:${PORT}/`);

  // The importer rewrites the bare specifier to a plugin route; following it
  // must land on the file's own URL (the normal pipeline), not a plugin-route
  // compile under a synthetic path.
  const main = await (await fetch(`http://localhost:${PORT}/src/main.js`)).text();
  const m = main.match(/from\s*"([^"]+)"/);
  assert.ok(m, `the bare import was rewritten:\n${main}`);
  const followed = await fetch(`http://localhost:${PORT}${m[1]}`);
  assert.equal(new URL(followed.url).pathname, "/src/tree.gen.js", "the plugin-resolved file serves at its own URL");
  assert.match(await followed.text(), /tree-1/);

  browser = await chromium.launch();
  const page = await browser.newPage();
  await page.goto(`http://localhost:${PORT}/`, { timeout: 30000 });
  await page.waitForFunction(() => window.__READY === true, { timeout: 20000 });
  assert.equal(await page.evaluate(() => window.__LABEL), "tree-1");

  // The edit must reach the page (nothing accepts the module, so a full
  // reload): before, the file was not watched and nothing happened.
  await sleep(300);
  w("src/tree.gen.js", `export const label = "tree-2";\n`);
  const deadline = Date.now() + 20000;
  for (;;) {
    let label;
    try {
      label = await page.evaluate(() => window.__LABEL);
    } catch {} // navigation in flight
    if (label === "tree-2") break;
    assert.ok(Date.now() < deadline, `the edit never reached the page (label: ${label})`);
    await sleep(250);
  }
  console.log("plugin-file-hmr e2e PASSED");
} catch (err) {
  failed = true;
  console.error("plugin-file-hmr e2e FAILED:", err.message);
} finally {
  if (browser) await browser.close().catch(() => {});
  srv.kill("SIGKILL");
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
