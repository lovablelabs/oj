// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// A plugin whose transform or load throws must fail the module: in dev the
// request is a 500 naming the plugin and the location (and the overlay gets it);
// in the build the process exits non-zero. Serving or bundling the raw source
// instead ships wrong code silently.

import { spawn, execSync, spawnSync } from "node:child_process";
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
const PORT = 5501;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-plugerr-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "plugerr", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "bad.js"), `export const B = "RAW_SOURCE_MARKER";\nexport const C = 2;\n`);
fs.writeFileSync(path.join(app, "src", "virt-user.js"), `import "virtual:boom";\nexport const V = 1;\n`);
fs.writeFileSync(path.join(app, "src", "fine.js"), `export const F = 1;\n`);
fs.writeFileSync(path.join(app, "src", "main.js"), `import "./bad.js";\nimport "./fine.js";\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `export default [{
    name: "bad-plugin",
    resolveId(id) { return id === "virtual:boom" ? "\\0boom" : null; },
    load(id) { if (id === "\\0boom") throw new Error("load exploded"); return null; },
    transform(code, id) {
      if (id.split("?")[0].endsWith("bad.js")) this.error({ message: "transform exploded", loc: { line: 2, column: 3 }, frame: "2 | export const C = 2;\\n  |   ^\\n" });
      return null;
    },
  }];\n`,
);

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${PORT}/`);

  const bad = await fetch(`http://localhost:${PORT}/src/bad.js`);
  const body = await bad.text();
  assert.equal(bad.status, 500, `throwing transform must 500, got ${bad.status}: ${body.slice(0, 200)}`);
  assert.match(body, /\[plugin:bad-plugin\] transform exploded/, `error names the plugin:\n${body}`);
  assert.match(body, /bad\.js:2:3/, `error carries the location:\n${body}`);
  assert.match(body, /2 \| export const C = 2;/, `error carries the code frame:\n${body}`);
  assert.doesNotMatch(body, /RAW_SOURCE_MARKER/, "raw source must not be served");

  const virt = await fetch(`http://localhost:${PORT}/src/virt-user.js`);
  const vbody = await virt.text();
  // The importer compiles; the virtual module it imports fails on fetch.
  const virtUrl = vbody.match(/from\s+"([^"]+)"/)?.[1] ?? vbody.match(/import\s+"([^"]+)"/)?.[1];
  assert.ok(virtUrl, `importer should reference the virtual module url:\n${vbody}`);
  const vmod = await fetch(`http://localhost:${PORT}${virtUrl}`);
  assert.equal(vmod.status, 500, "throwing load must 500");
  assert.match(await vmod.text(), /\[plugin:bad-plugin\] load exploded/, "load error names the plugin");

  const fine = await fetch(`http://localhost:${PORT}/src/fine.js`);
  assert.equal(fine.status, 200, "unaffected modules still serve");
  srv.kill("SIGKILL");
  await sleep(300);

  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  const r = spawnSync(oj, ["build", app], { encoding: "utf8" });
  assert.notEqual(r.status, 0, "build must exit non-zero when a plugin transform throws");
  assert.match(r.stderr + r.stdout, /bad-plugin/, `build error names the plugin:\n${r.stderr}`);
  assert.ok(
    !fs.existsSync(path.join(app, "dist", "index.html")) ||
      !fs
        .readdirSync(path.join(app, "dist", "assets"))
        .some((f) => fs.readFileSync(path.join(app, "dist", "assets", f), "utf8").includes("RAW_SOURCE_MARKER")),
    "no output with the raw source",
  );
  console.log("PLUGIN-ERROR-SURFACED E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PLUGIN-ERROR-SURFACED E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
