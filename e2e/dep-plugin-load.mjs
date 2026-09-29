// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Plugins and node_modules: (1) an object-form `load` whose `filter.id` names a
// dependency is offered that dependency (Vite runs load for every module; oj
// gates deps on the plugin's own filter so unfiltered plugins cost no RPC);
// (2) a linked workspace package (realpath outside node_modules) is source, so a
// plugin transform applies to it; (3) a transform emitting an import of an absolute
// path OUTSIDE the app root is served through /@fs, not left as a 404 url.

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
const PORT = 5530;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-dep-load-"));
const app = path.join(tmp, "app");
const linked = path.join(tmp, "packages", "ui"); // realpath outside app and outside node_modules
const outside = path.join(tmp, "shared", "util.js"); // absolute path a plugin resolves to
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.mkdirSync(path.join(app, "node_modules", "fake-dep"), { recursive: true });
fs.mkdirSync(linked, { recursive: true });
fs.mkdirSync(path.dirname(outside), { recursive: true });
fs.writeFileSync(
  path.join(app, "package.json"),
  JSON.stringify({ name: "dep-load-app", version: "1.0.0", dependencies: { "fake-dep": "1.0.0", "@ws/ui": "1.0.0" } }),
);
fs.writeFileSync(
  path.join(app, "node_modules", "fake-dep", "package.json"),
  JSON.stringify({ name: "fake-dep", version: "1.0.0", main: "index.js", type: "module" }),
);
fs.writeFileSync(path.join(app, "node_modules", "fake-dep", "index.js"), `export const WHO = "FROM_DISK";\n`);
fs.writeFileSync(
  path.join(linked, "package.json"),
  JSON.stringify({ name: "@ws/ui", version: "1.0.0", main: "index.js", type: "module" }),
);
fs.writeFileSync(path.join(linked, "index.js"), `export const UI = "__UI_MARKER__";\n`);
fs.mkdirSync(path.join(app, "node_modules", "@ws"), { recursive: true });
fs.symlinkSync(linked, path.join(app, "node_modules", "@ws", "ui"), "dir");
fs.writeFileSync(
  path.join(path.dirname(outside), "package.json"),
  JSON.stringify({ name: "shared", version: "1.0.0", type: "module" }),
);
fs.writeFileSync(outside, `export const OUTSIDE = "outside-root";\n`);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import { WHO } from "fake-dep";\nimport { UI } from "@ws/ui";\nimport { OUTSIDE } from "virtual:outside";\nwindow.__R = [WHO, UI, OUTSIDE];\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);
// Like Vite, plugin hooks apply to deps that are NOT pre-bundled (optimizeDeps.exclude).
fs.writeFileSync(path.join(app, "oj.config.json"), JSON.stringify({ optimizeDeps: { exclude: ["fake-dep"] } }));
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `export default [{
     name: "dep-aware",
     load: {
       filter: { id: /node_modules\\/fake-dep\\// },
       handler(id) { return id.includes("fake-dep") ? "export const WHO = \\"FROM_PLUGIN\\";\\n" : null; },
     },
     transform(code, id) {
       if (code.includes("__UI_MARKER__")) return code.replace("__UI_MARKER__", "TRANSFORMED_LINKED");
       // A transform emitting an ABSOLUTE path outside the app root (a monorepo
       // sibling), as Vite plugins may: oj must serve it via /@fs.
       if (code.includes("virtual:outside")) return code.replace(${JSON.stringify(JSON.stringify("virtual:outside"))}, ${JSON.stringify(JSON.stringify(outside))});
       return null;
     },
   }];\n`,
);

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${PORT}/`);
  const main = await (await fetch(`http://localhost:${PORT}/src/main.js`)).text();
  const urls = [...main.matchAll(/from\s+"([^"]+)"/g)].map((m) => m[1]);
  assert.equal(urls.length, 3, `three rewritten imports:\n${main}`);
  const [depUrl, linkedUrl, outsideUrl] = urls;

  const dep = await (await fetch(`http://localhost:${PORT}${depUrl}`)).text();
  assert.match(dep, /FROM_PLUGIN/, `filtered plugin load did not run for the dep (${depUrl}):\n${dep.slice(0, 200)}`);

  const ui = await (await fetch(`http://localhost:${PORT}${linkedUrl}`)).text();
  assert.match(ui, /TRANSFORMED_LINKED/, `plugin transform skipped the linked package:\n${ui.slice(0, 200)}`);

  assert.match(outsideUrl, /^\/@fs\//, `outside-root path not served via /@fs: ${outsideUrl}`);
  const res = await fetch(`http://localhost:${PORT}${outsideUrl}`);
  assert.equal(res.status, 200, `outside-root module not served: ${res.status}`);
  assert.match(await res.text(), /outside-root/);
  console.log("DEP-PLUGIN-LOAD E2E PASSED");
} catch (err) {
  failed = true;
  console.error("DEP-PLUGIN-LOAD E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(tmp, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
