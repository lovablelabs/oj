// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite resolves with a `development` condition in dev and `production` in build.
// A dep whose exports map branches on it must resolve the same way in the dev
// server, in the pre-bundle (optimizeDeps.include), and in the build; and a
// dynamic import() of a CommonJS dep must get the namespace interop wrapper.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const PORT = 5531;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-devcond-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
const dep = path.join(app, "node_modules", "cond-dep");
fs.mkdirSync(dep, { recursive: true });
fs.writeFileSync(
  path.join(app, "package.json"),
  JSON.stringify({ name: "devcond-app", version: "1.0.0", dependencies: { "cond-dep": "1.0.0", "cjs-dep": "1.0.0" } }),
);
fs.writeFileSync(
  path.join(dep, "package.json"),
  JSON.stringify({
    name: "cond-dep",
    version: "1.0.0",
    type: "module",
    exports: { ".": { development: "./dev.js", default: "./prod.js" } },
  }),
);
fs.writeFileSync(path.join(dep, "dev.js"), `export const FLAVOR = "DEV_BUILD";\n`);
fs.writeFileSync(path.join(dep, "prod.js"), `export const FLAVOR = "PROD_BUILD";\n`);
const cjs = path.join(app, "node_modules", "cjs-dep");
fs.mkdirSync(cjs, { recursive: true });
fs.writeFileSync(
  path.join(cjs, "package.json"),
  JSON.stringify({ name: "cjs-dep", version: "1.0.0", main: "index.js" }),
);
fs.writeFileSync(path.join(cjs, "index.js"), `exports.answer = 42;\n`);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import { FLAVOR } from "cond-dep";\nwindow.__FLAVOR = FLAVOR;\nimport("cjs-dep").then((m) => { window.__ANSWER = m.answer; window.__DEFAULT = m.default && m.default.answer; });\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

let failed = false;
let srv = null;
try {
  // Dev: source import resolves the `development` export.
  srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
  await waitUp(`http://localhost:${PORT}/`);
  const main = await (await fetch(`http://localhost:${PORT}/src/main.js`)).text();
  const depUrl = main.match(/from\s+"([^"]+)"/)[1];
  const served = await (await fetch(`http://localhost:${PORT}${depUrl}`)).text();
  assert.match(served, /DEV_BUILD/, `dev did not pick the development export:\n${served.slice(0, 300)}`);
  assert.match(
    main,
    /import\("[^"]+"\)\.then\(__oj_dyn_interop\)/,
    `dynamic import of a CJS dep is not interop-wrapped:\n${main}`,
  );
  assert.match(main, /const __oj_dyn_interop = /, "interop helper missing");
  srv.kill("SIGKILL");
  srv = null;
  await sleep(300);

  // Dev with the dep pre-bundled (optimizeDeps.include): the sidecar must pick the SAME file.
  // The optimizer runs esbuild from the app; borrow the fixture install like dep-optimize.mjs.
  const esbuildSrc = path.join(repo, "e2e/fixtures/start-app/node_modules/esbuild");
  const esbuildScoped = path.join(repo, "e2e/fixtures/start-app/node_modules/@esbuild");
  if (!fs.existsSync(esbuildSrc))
    throw new Error("esbuild fixture not installed: (cd e2e/fixtures/start-app && npm install)");
  fs.symlinkSync(esbuildSrc, path.join(app, "node_modules", "esbuild"));
  if (fs.existsSync(esbuildScoped)) fs.symlinkSync(esbuildScoped, path.join(app, "node_modules", "@esbuild"));
  fs.writeFileSync(path.join(app, "oj.config.json"), JSON.stringify({ optimizeDeps: { include: ["cond-dep"] } }));
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
  await waitUp(`http://localhost:${PORT}/`);
  let bundled = null;
  await settles(
    async () => {
      const m = await (await fetch(`http://localhost:${PORT}/src/main.js`)).text();
      const u = m.match(/from\s+"(\/@oj-deps\/[^"]+)"/);
      if (u) bundled = await (await fetch(`http://localhost:${PORT}${u[1]}`)).text();
      return Boolean(bundled);
    },
    { pollMs: 200 },
  );
  assert.ok(bundled, "cond-dep was not pre-bundled from optimizeDeps.include");
  assert.match(
    bundled,
    /DEV_BUILD/,
    `pre-bundle picked a different file than the dev server:\n${bundled.slice(0, 300)}`,
  );
  srv.kill("SIGKILL");
  srv = null;
  fs.rmSync(path.join(app, "oj.config.json"));

  // Build: the `production` condition.
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const assets = path.join(app, "dist", "assets");
  const built = fs
    .readdirSync(assets)
    .filter((f) => f.endsWith(".js"))
    .map((f) => fs.readFileSync(path.join(assets, f), "utf8"))
    .join("\n");
  assert.match(built, /PROD_BUILD/, "build did not pick the production export");
  assert.doesNotMatch(built, /DEV_BUILD/, "build picked the development export");
  console.log("DEV-CONDITION E2E PASSED");
} catch (err) {
  failed = true;
  console.error("DEV-CONDITION E2E FAILED:", err.message);
} finally {
  if (srv) srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
