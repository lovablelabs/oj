// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite runs every plugin's `load`/`transform` on a dependency it does not
// pre-bundle (an `optimizeDeps.exclude`d package), function-form hooks with no
// filter included. oj serves deps per file and only offers them to plugins whose
// object-form filter asks, standing in for Vite's pre-bundle; an excluded package
// must instead go through the full plugin pipeline like app source.

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
const PORT = Number(process.env.OJ_E2E_PORT || 5535);

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-excldep-"));
const write = (rel, text) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), text);
};
write(
  "package.json",
  JSON.stringify({
    name: "excldep-app",
    version: "1.0.0",
    dependencies: { "excl-dep": "1.0.0", "plain-dep": "1.0.0" },
  }),
);
write("oj.config.json", JSON.stringify({ optimizeDeps: { exclude: ["excl-dep"] } }));
for (const name of ["excl-dep", "plain-dep"]) {
  write(
    `node_modules/${name}/package.json`,
    JSON.stringify({ name, version: "1.0.0", type: "module", main: "index.js" }),
  );
  write(`node_modules/${name}/index.js`, `export const tag = "__MARKER__";\nexport { helper } from "./helper.js";\n`);
  write(`node_modules/${name}/helper.js`, `export const helper = "ON_DISK";\n`);
}
write(
  "src/main.js",
  `import { tag as a } from "excl-dep";\nimport { tag as b } from "plain-dep";\nconsole.log(a, b);\n`,
);
write(
  "index.html",
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);
// Function-form hooks: no `filter`, so oj learns nothing about which ids they want.
write(
  "oj.plugins.mjs",
  `export default [{
     name: "dep-rewriter",
     load(id) {
       if (id.split("?")[0].endsWith("/helper.js")) return "export const helper = \\"FROM_LOAD\\";\\n";
       return null;
     },
     transform(code, id) {
       if (code.includes("__MARKER__")) return { code: code.replace("__MARKER__", "FROM_TRANSFORM"), map: null };
       return null;
     },
   }];\n`,
);

let failed = false;
let srv = null;
try {
  srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
  await waitUp(`http://localhost:${PORT}/`);
  const get = async (u) => {
    const r = await fetch(`http://localhost:${PORT}${u}`);
    const t = await r.text();
    assert.equal(r.status, 200, `${u}: ${t.slice(0, 200)}`);
    return t;
  };

  // Excluded package: function-form transform and load both run on its files.
  const excl = await get("/node_modules/excl-dep/index.js");
  assert.match(excl, /FROM_TRANSFORM/, `excluded dep skipped the plugin transform:\n${excl}`);
  const exclHelper = await get("/node_modules/excl-dep/helper.js");
  assert.match(exclHelper, /FROM_LOAD/, `excluded dep skipped the plugin load:\n${exclHelper}`);

  // A regular dep stands in for Vite's pre-bundle: untouched, as no plugin asked.
  const plain = await get("/node_modules/plain-dep/index.js");
  assert.match(plain, /__MARKER__/, `plain dep unexpectedly went through the plugin transform:\n${plain}`);
  const plainHelper = await get("/node_modules/plain-dep/helper.js");
  assert.match(plainHelper, /ON_DISK/, `plain dep unexpectedly went through the plugin load:\n${plainHelper}`);
  console.log("PLUGIN-EXCLUDED-DEP E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PLUGIN-EXCLUDED-DEP E2E FAILED:", err.stack || err.message);
} finally {
  if (srv) srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
