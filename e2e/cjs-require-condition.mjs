// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite resolves a `require()` with the `require` export condition and an
// `import` with `import` (resolve.ts getConditions, isRequire). A CommonJS dep
// served per file that requires a dual package must therefore get its CJS build
// (`module.exports = fn` -> `fn`), while the app's own ESM import of the same
// package still gets the ESM build.

import { spawn, execSync } from "node:child_process";
import { createRequire } from "node:module";
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
const PORT = Number(process.env.OJ_E2E_PORT || 5534);
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-reqcond-"));
const write = (rel, text) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), text);
};
write(
  "package.json",
  JSON.stringify({ name: "reqcond-app", version: "1.0.0", dependencies: { "dual-fn": "1.0.0", "cjs-user": "1.0.0" } }),
);
write(
  "node_modules/dual-fn/package.json",
  JSON.stringify({
    name: "dual-fn",
    version: "1.0.0",
    exports: {
      ".": { development: { import: "./esm.js", require: "./cjs.js" }, import: "./esm.js", require: "./cjs.js" },
    },
  }),
);
write("node_modules/dual-fn/esm.js", `export default function fn() { return "ESM_BUILD"; }\n`);
write("node_modules/dual-fn/cjs.js", `module.exports = function fn() { return "CJS_BUILD"; };\n`);
write("node_modules/cjs-user/package.json", JSON.stringify({ name: "cjs-user", version: "1.0.0", main: "index.js" }));
write(
  "node_modules/cjs-user/index.js",
  `var fn = require("dual-fn");\nexports.kind = typeof fn;\nexports.value = typeof fn === "function" ? fn() : String(fn && fn.default);\n`,
);
write(
  "src/main.js",
  `import { kind, value } from "cjs-user";\nimport esmFn from "dual-fn";\nwindow.__R = { kind, value, esm: esmFn() };\n`,
);
write(
  "index.html",
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

let failed = false;
let srv = null;
let browser = null;
try {
  srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
  await waitUp(`http://localhost:${PORT}/`);

  // The ESM importer keeps the `import` condition.
  const main = await (await fetch(`http://localhost:${PORT}/src/main.js`)).text();
  assert.match(
    main,
    /from\s+"\/node_modules\/dual-fn\/esm\.js[^"]*"/,
    `app import should pick the ESM build:\n${main}`,
  );
  const userUrl = main.match(/from\s+"(\/node_modules\/cjs-user\/index\.js[^"]*)"/)?.[1];
  assert.ok(userUrl, `cjs-user not rewritten to its served URL:\n${main}`);

  // The CJS requirer resolves with the `require` condition.
  const user = await (await fetch(`http://localhost:${PORT}${userUrl}`)).text();
  assert.match(
    user,
    /from\s+"\/node_modules\/dual-fn\/cjs\.js[^"]*"/,
    `require("dual-fn") should pick the CJS build:\n${user}`,
  );
  assert.doesNotMatch(user, /dual-fn\/esm\.js/, `require("dual-fn") picked the ESM build:\n${user}`);

  // And at runtime the requirer sees `module.exports` itself, not a namespace.
  browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  await page.goto(`http://localhost:${PORT}/`, { timeout: 30000 });
  let result = null;
  await settles(async () => (result = await page.evaluate(() => window.__R || null)));
  assert.deepEqual(errors, [], `page errors: ${errors.join("; ")}`);
  assert.deepEqual(result, { kind: "function", value: "CJS_BUILD", esm: "ESM_BUILD" });
  console.log("CJS-REQUIRE-CONDITION E2E PASSED");
} catch (err) {
  failed = true;
  console.error("CJS-REQUIRE-CONDITION E2E FAILED:", err.stack || err.message);
} finally {
  if (browser) await browser.close();
  if (srv) srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
