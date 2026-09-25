// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// An ES-module dependency importing an unbundled UMD sibling, served file by
// file (the proj4 -> geographiclib-geodesic shape): the UMD's names are
// assigned at runtime (`module.exports = factory()`), so static CJS analysis
// exposes none of them and the browser strict-links the ESM dep's
// `import { Geodesic } from "geodesiclike"` to nothing. Served ESM deps now
// get the same importer-side CJS interop as app source: named imports read
// off module.exports at runtime, and `export * as ns` re-exports build the
// interop namespace. Run with a built target/debug/oj (playwright from e2e/).

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-esm-umd-"));
const write = (rel, contents) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), contents);
};
write("package.json", JSON.stringify({ name: "esm-umd-app", version: "1.0.0" }));
// The UMD dep: exports assigned at runtime by the factory, invisible to
// static CJS export analysis.
write(
  "node_modules/geodesiclike/package.json",
  JSON.stringify({ name: "geodesiclike", version: "1.0.0", main: "g.min.js" }),
);
write(
  "node_modules/geodesiclike/g.min.js",
  `(function(cb){var geodesic={Geodesic:{WGS84:{a:6378137}}};cb(geodesic);})(function(geo){` +
    `if(typeof module==="object"&&module.exports){module.exports=geo;}` +
    `else if(typeof define==="function"&&define.amd){define([],function(){return geo;});}` +
    `else{window.geodesic=geo;}});\n`,
);
// The ESM dep in between: a named import off the UMD, plus a namespace star
// re-export of it (the barrel shape `export * as ns`).
write(
  "node_modules/proj4like/package.json",
  JSON.stringify({ name: "proj4like", version: "1.0.0", type: "module", main: "index.js" }),
);
write(
  "node_modules/proj4like/index.js",
  `import { Geodesic } from "geodesiclike";\n` +
    `export * as geo from "geodesiclike";\n` +
    `export const semiMajor = () => Geodesic.WGS84.a;\n`,
);
write(
  "src/main.js",
  `import { semiMajor, geo } from "proj4like";\n` +
    `window.__RESULT = semiMajor();\n` +
    `window.__NS = geo.default && geo.default.Geodesic ? geo.default.Geodesic.WGS84.a : undefined;\n` +
    `window.__READY = true;\n`,
);
write(
  "index.html",
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

const port = 5461;
const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
const exited = new Promise((r) => srv.on("exit", r));
let failed = false;
try {
  for (let i = 0; i < 80; i++) {
    try {
      if ((await fetch(`http://localhost:${port}/`)).ok) break;
    } catch {}
    await sleep(200);
  }
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${port}/`, { timeout: 30000 });
    await page.waitForFunction(() => window.__READY === true, { timeout: 10000 }).catch(() => {});
    const { result, ns } = await page.evaluate(() => ({ result: window.__RESULT, ns: window.__NS }));
    const bad = [];
    if (errors.length) bad.push(`page errors ${errors.join("|")}`);
    if (result !== 6378137) bad.push(`named import through the UMD: __RESULT ${result}`);
    if (ns !== 6378137) bad.push(`export * as ns interop namespace: __NS ${ns}`);
    if (bad.length) throw new Error(bad.join("; "));
    console.log(`Geodesic read through the UMD dep, named + star-as: ${result} OK`);
  } finally {
    await browser.close();
  }
} catch (err) {
  failed = true;
  console.error("DEP ESM->UMD INTEROP E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await exited;
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
