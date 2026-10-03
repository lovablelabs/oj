// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// A Vite 8 app (its vite brings rolldown; no esbuild anywhere in the app):
// oj discovers the deps the app imports and pre-bundles them with that
// rolldown, the way Vite 8's optimizer does. React, a many-file ESM icon
// package and a CJS dep must all be served from /@oj-deps (one request per
// dep, not one per file), and the page must render in a real browser. An
// import only a config plugin can resolve is discovered too: the scan goes
// through the app's plugins with `scan: true`, as in Vite.

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
const fixtureModules = path.join(repo, "e2e/fixtures/start-app/node_modules");
const port = 5274;
const ICONS = 40;

if (!fs.existsSync(path.join(fixtureModules, "vite", "package.json"))) {
  console.log("SKIP dep-optimize-rolldown: start-app fixture not installed");
  console.log("  enable with: npm ci --prefix e2e/fixtures/start-app");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-optdep-rd-"));
const nm = path.join(app, "node_modules");
fs.mkdirSync(nm, { recursive: true });
for (const dep of ["vite", "react", "react-dom", "scheduler"]) {
  fs.symlinkSync(path.join(fixtureModules, dep), path.join(nm, dep));
}
const write = (rel, content) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), content);
};
// lucide-react's shape: an ESM barrel re-exporting one module per icon.
write(
  "node_modules/icons/package.json",
  JSON.stringify({ name: "icons", version: "1.0.0", type: "module", module: "index.js", main: "index.js" }),
);
let barrel = "";
for (let i = 0; i < ICONS; i++) {
  write(
    `node_modules/icons/icons/icon${i}.js`,
    `import { createElement } from "react";\nexport default function Icon${i}() { return createElement("i", null, "${i}"); }\n`,
  );
  barrel += `export { default as Icon${i} } from "./icons/icon${i}.js";\n`;
}
write("node_modules/icons/index.js", barrel);
write("node_modules/cjs-lib/package.json", JSON.stringify({ name: "cjs-lib", version: "1.0.0", main: "index.js" }));
write(
  "node_modules/cjs-lib/index.js",
  `"use strict";\n` +
    `Object.defineProperty(exports, "__esModule", { value: true });\n` +
    `Object.defineProperty(exports, "greet", { enumerable: true, get: function () { return greet; } });\n` +
    `function greet(n) { return "hi " + n; }\n`,
);
write("package.json", JSON.stringify({ name: "optdep-rd-app", private: true, type: "module" }));
const scanMarker = path.join(app, "scan-flag.txt");
write(
  "vite.config.js",
  `import fs from "node:fs";\n` +
    `export default { plugins: [{ name: "app-icons", resolveId(id, importer, opts) {\n` +
    `  if (id !== "app-icons") return null;\n` +
    `  if (opts && opts.scan) fs.writeFileSync(${JSON.stringify(scanMarker)}, "scan");\n` +
    `  return ${JSON.stringify(path.join(app, "node_modules", "icons", "index.js"))};\n` +
    `} }] };\n`,
);
write(
  "index.html",
  `<!doctype html><html><head><title>t</title></head><body><div id="root"></div><script type="module" src="/main.jsx"></script></body></html>`,
);
write(
  "main.jsx",
  `import { createRoot } from "react-dom/client";\n` +
    `import { Icon0, Icon7 } from "icons";\n` +
    `import { Icon3 } from "app-icons";\n` +
    `import { greet } from "cjs-lib";\n` +
    `function App() { return <p id="out">{greet("world")} <Icon0 /><Icon7 /><Icon3 /></p>; }\n` +
    `createRoot(document.getElementById("root")).render(<App />);\n`,
);

let server;
let failed = false;
try {
  assert.throws(() => createRequire(path.join(app, "package.json")).resolve("esbuild"), "the app has no esbuild");

  server = spawn(oj, ["dev", app, "--port", String(port)], { stdio: ["ignore", "inherit", "inherit"] });
  await waitUp(`http://localhost:${port}/`, { proc: server });

  const main = await (await fetch(`http://localhost:${port}/main.jsx`)).text();
  for (const dep of ["react-dom_client", "icons", "cjs-lib"]) {
    assert.match(
      main,
      new RegExp(`"/@oj-deps/${dep}(__oj_named)?\\.mjs\\?v=[0-9a-f]{8}"`),
      `${dep} pre-bundled:\n${main.slice(0, 600)}`,
    );
  }
  const iconsUrl = main.match(/"(\/@oj-deps\/icons\.mjs\?v=[0-9a-f]{8})"/)[1];
  // `icons` and the plugin-resolved `app-icons` are one file: both entries
  // re-export one shared chunk (one module instance), so follow its imports.
  let icons = await (await fetch(`http://localhost:${port}${iconsUrl}`)).text();
  for (const [, rel] of icons.matchAll(/from "\.\/([^"]+)"/g)) {
    icons += await (await fetch(`http://localhost:${port}/@oj-deps/${rel}`)).text();
  }
  assert.match(icons, /\/\/#region/, "bundled by rolldown");
  assert.match(icons, /function Icon39\b/, "the whole barrel is in one module");

  const manifest = JSON.parse(fs.readFileSync(path.join(app, ".oj-cache", "v1", "deps", "manifest.json"), "utf8"));
  for (const dep of ["react-dom/client", "icons", "app-icons", "cjs-lib", "react/jsx-dev-runtime"]) {
    assert.ok(manifest.metadata[dep], `${dep} in the prebundle manifest: ${Object.keys(manifest.metadata)}`);
  }
  assert.ok(fs.existsSync(scanMarker), "the scan asked the plugin with scan: true");

  let chromium;
  try {
    ({ chromium } = await import("playwright"));
  } catch {
    console.error("  (playwright not installed locally; skipped the browser render check; CI covers it)");
  }
  if (chromium) {
    const browser = await chromium.launch();
    try {
      const page = await browser.newPage();
      const requests = [];
      const errors = [];
      page.on("request", (r) => requests.push(new URL(r.url()).pathname));
      page.on("pageerror", (e) => errors.push(e.message));
      page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
      await page.goto(`http://localhost:${port}/`);
      await page.waitForFunction(() => document.getElementById("out")?.textContent.includes("hi world"), null, {
        timeout: 30000,
      });
      assert.equal(await page.textContent("#out"), "hi world 073");
      assert.deepEqual(errors, [], "no page errors");
      const fromNodeModules = requests.filter((p) => p.includes("/node_modules/"));
      assert.deepEqual(fromNodeModules, [], "no dep file is served one by one");
      assert.ok(
        requests.some((p) => p.startsWith("/@oj-deps/")),
        "deps come from the prebundle",
      );
      console.log(`  rendered with ${requests.length} requests`);
    } finally {
      await browser.close();
    }
  }
  console.log("dep-optimize-rolldown e2e PASSED");
} catch (err) {
  failed = true;
  console.error("dep-optimize-rolldown e2e FAILED:", err.message);
} finally {
  if (server) server.kill("SIGKILL");
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
