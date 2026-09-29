// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

// Vite runs plugin `load` hooks before reading a module from disk (the fs read is
// its last-resort `vite:load-fallback` plugin), so a plugin can replace an
// existing on-disk file's contents. The real app's i18n-dev plugin depends on
// this to collapse a generated barrel into grouped virtual modules. oj must call
// `load` first for app source and only fall back to the disk read when no plugin
// loads. Here a plugin overrides an on-disk file; the plugin's version must win.
const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-loadfirst-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "loadfirst-app", version: "1.0.0" }));
// The on-disk file whose contents a plugin overrides via `load`.
fs.writeFileSync(path.join(app, "src", "data.js"), `export const VALUE = "FROM_DISK";\n`);
// A second real file the plugin does NOT load: it must be read from disk.
fs.writeFileSync(path.join(app, "src", "plain.js"), `export const PLAIN = "REAL_DISK";\n`);
fs.writeFileSync(path.join(app, "src", "entry.js"), `import "./data.js";\nimport "./plain.js";\nconsole.log("app");\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/entry.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `export default [{
     name: "override-loader",
     enforce: "pre",
     load(id) {
       if (id.split("?")[0].endsWith("data.js")) {
         return "export const VALUE = \\"FROM_PLUGIN\\";\\n";
       }
       return null;
     },
   }];\n`,
);

let failed = false;
const port = 5397;
const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${port}/`);

  const data = await (await fetch(`http://localhost:${port}/src/data.js`)).text();
  if (!/FROM_PLUGIN/.test(data)) throw new Error(`plugin load did not run before the disk read: ${data.slice(0, 80)}`);
  if (/FROM_DISK/.test(data)) throw new Error("disk contents leaked past the plugin load");

  // A file no plugin loads still reads from disk.
  const plain = await (await fetch(`http://localhost:${port}/src/plain.js`)).text();
  if (!/REAL_DISK/.test(plain)) throw new Error(`unloaded file was not read from disk: ${plain.slice(0, 80)}`);

  console.log("PLUGIN-LOAD-FIRST E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PLUGIN-LOAD-FIRST E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
