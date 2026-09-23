// SPDX-License-Identifier: MIT

// An ES-module dependency with a named import from a UMD dependency, neither
// pre-bundled (the proj4 -> geographiclib-geodesic shape: proj4's
// lib/projections/aeqd.js does `import { Geodesic } from
// "geographiclib-geodesic"`, whose `module.exports = factory()` names static
// CJS analysis cannot see). Served file by file, the browser strict-linked the
// ESM file against the wrapped UMD and failed with "does not provide an export
// named 'Geodesic'"; the ESM dep now gets the same importer-side interop as app
// source, so the name is read off module.exports at runtime.
// Run with a built target/debug/oj (needs playwright from e2e/).

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-esm-umd-"));
const write = (rel, contents) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), contents);
};
write("package.json", JSON.stringify({ name: "esm-umd-app", version: "1.0.0" }));
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
write(
  "node_modules/proj4like/package.json",
  JSON.stringify({ name: "proj4like", version: "1.0.0", type: "module", main: "index.js" }),
);
write(
  "node_modules/proj4like/index.js",
  `import { Geodesic } from "geodesiclike";\nexport const semiMajor = () => Geodesic.WGS84.a;\n`,
);
write("src/main.js", `import { semiMajor } from "proj4like";\nwindow.__RESULT = semiMajor();\n`);
write(
  "index.html",
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

async function check(mode, port) {
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  const args = ["dev", app, "--port", String(port)];
  if (mode === "bundle") args.push("--bundle");
  const srv = spawn(oj, args, { stdio: "ignore" });
  for (let i = 0; i < 80; i++) { try { if ((await fetch(`http://localhost:${port}/`)).ok) break; } catch {} await sleep(200); }
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto(`http://localhost:${port}/`, { timeout: 30000 });
    await page.waitForFunction(() => window.__RESULT !== undefined, { timeout: 10000 }).catch(() => {});
    const result = await page.evaluate(() => window.__RESULT);
    const bad = [];
    if (errors.length) bad.push(`page errors ${errors.join("|")}`);
    if (result !== 6378137) bad.push(`__RESULT ${result}`);
    if (bad.length) throw new Error(`[${mode}] ${bad.join("; ")}`);
    console.log(`[${mode}] Geodesic read through the UMD dep: ${result} OK`);
  } finally {
    await browser.close();
    srv.kill("SIGKILL");
  }
}

let failed = false;
try {
  await check("non-bundle", 5395);
  await check("bundle", 5396);
  console.log("DEP ESM->UMD INTEROP E2E PASSED (both modes)");
} catch (err) {
  failed = true;
  console.error("DEP ESM->UMD INTEROP E2E FAILED:", err.message);
} finally {
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
