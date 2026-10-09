// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// An optimized dep that another optimized dep imports is still one module.
// Rolldown links the importing bundle to the entry chunk as `./cursor.mjs`,
// which the browser resolves without the `?v=` the app's own import of that
// entry carries, so the entry used to evaluate twice (ProseMirror apps died on
// `Duplicate use of selection JSON ID gapcursor`). Every dep-internal import of
// an entry, static or dynamic, must resolve to the app's URL for it, and the
// page must see one class from one evaluation.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { rmrf, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const fixtureModules = path.join(repo, "e2e/fixtures/start-app/node_modules");
const port = 5275;
const origin = `http://localhost:${port}`;

if (!fs.existsSync(path.join(fixtureModules, "vite", "package.json"))) {
  console.log("SKIP dep-optimize-entry-instance: start-app fixture not installed");
  console.log("  enable with: npm ci --prefix e2e/fixtures/start-app");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-optdep-entry-"));
const write = (rel, content) => {
  fs.mkdirSync(path.dirname(path.join(app, rel)), { recursive: true });
  fs.writeFileSync(path.join(app, rel), content);
};
fs.mkdirSync(path.join(app, "node_modules"), { recursive: true });
fs.symlinkSync(path.join(fixtureModules, "vite"), path.join(app, "node_modules", "vite"));
const esmPackage = (name) =>
  JSON.stringify({ name, version: "1.0.0", type: "module", module: "index.js", main: "index.js" });
// prosemirror-gapcursor's shape: a module whose body must run once.
write("node_modules/cursor/package.json", esmPackage("cursor"));
write(
  "node_modules/cursor/index.js",
  `globalThis.__cursorEvals = (globalThis.__cursorEvals ?? 0) + 1;\nexport class Cursor {}\n`,
);
// @tiptap/extensions' shape: another optimized dep importing it statically.
write("node_modules/ext/package.json", esmPackage("ext"));
write("node_modules/ext/index.js", `import { Cursor } from "cursor";\nexport const cursorOf = () => Cursor;\n`);
// And one reaching it through a dynamic import.
write("node_modules/lazy-ext/package.json", esmPackage("lazy-ext"));
write("node_modules/lazy-ext/index.js", `export const loadCursor = () => import("cursor");\n`);
write("package.json", JSON.stringify({ name: "optdep-entry-app", private: true, type: "module" }));
write(
  "index.html",
  `<!doctype html><html><head><title>t</title></head><body><p id="out"></p><script type="module" src="/main.js"></script></body></html>`,
);
write(
  "main.js",
  `import { Cursor } from "cursor";\n` +
    `import { cursorOf } from "ext";\n` +
    `import { loadCursor } from "lazy-ext";\n` +
    `const lazy = await loadCursor();\n` +
    `document.getElementById("out").textContent = [cursorOf() === Cursor, lazy.Cursor === Cursor, globalThis.__cursorEvals].join(" ");\n`,
);

const RELATIVE_IMPORT = /\bfrom\s*"(\.\/[^"]+)"|\bimport\s*"(\.\/[^"]+)"|\bimport\(\s*"(\.\/[^"]+)"\s*\)/g;

// Every URL a dep bundle reaches `file` through, walking its relative imports.
async function urlsReaching(file, roots) {
  const found = { static: new Set(), dynamic: new Set() };
  const seen = new Set(roots);
  const queue = [...roots];
  while (queue.length) {
    const url = queue.shift();
    const res = await fetch(url);
    assert.equal(res.status, 200, `${url} serves`);
    const code = await res.text();
    for (const [, from, bare, dynamic] of code.matchAll(RELATIVE_IMPORT)) {
      const next = new URL(from ?? bare ?? dynamic, url).href;
      if (new URL(next).pathname === `/@oj-deps/${file}`) found[dynamic ? "dynamic" : "static"].add(next);
      if (!seen.has(next)) {
        seen.add(next);
        queue.push(next);
      }
    }
  }
  return found;
}

let server;
let failed = false;
try {
  server = spawn(oj, ["dev", app, "--port", String(port)], { stdio: ["ignore", "inherit", "inherit"] });
  await waitUp(`${origin}/`, { proc: server });

  const main = await (await fetch(`${origin}/main.js`)).text();
  const depUrl = (dep) => {
    const m = main.match(new RegExp(`"(/@oj-deps/${dep}\\.mjs\\?v=[0-9a-f]{8})"`));
    assert.ok(m, `${dep} pre-bundled:\n${main.slice(0, 600)}`);
    return `${origin}${m[1]}`;
  };
  const cursor = depUrl("cursor");
  const reached = await urlsReaching("cursor.mjs", [depUrl("ext"), depUrl("lazy-ext")]);
  assert.ok(reached.static.size > 0, "a dep bundle imports the cursor entry chunk statically");
  assert.ok(reached.dynamic.size > 0, "a dep bundle imports the cursor entry chunk dynamically");
  for (const url of [...reached.static, ...reached.dynamic]) {
    assert.equal(url, cursor, "a dep-internal import of an entry resolves to the app's URL for it");
  }

  let chromium;
  try {
    ({ chromium } = await import("playwright"));
  } catch {
    console.error("  (playwright not installed locally; skipped the browser check; CI covers it)");
  }
  if (chromium) {
    const browser = await chromium.launch();
    try {
      const page = await browser.newPage();
      const errors = [];
      page.on("pageerror", (e) => errors.push(e.message));
      await page.goto(`${origin}/`);
      await page.waitForFunction(() => document.getElementById("out")?.textContent, null, { timeout: 30000 });
      assert.equal(await page.textContent("#out"), "true true 1", "one class, one evaluation");
      assert.deepEqual(errors, [], "no page errors");
    } finally {
      await browser.close();
    }
  }
  console.log("dep-optimize-entry-instance e2e PASSED");
} catch (err) {
  failed = true;
  console.error("dep-optimize-entry-instance e2e FAILED:", err.message);
} finally {
  if (server) server.kill("SIGKILL");
  await rmrf(app);
}
process.exit(failed ? 1 : 0);
