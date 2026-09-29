// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// `base: "./"` must emit page-relative URLs (`./assets/…` from the root page,
// `../assets/…` from a nested page, sibling refs inside CSS), and dynamic
// imports must go through Vite's `__vitePreload`: the lazy chunk's shared
// chunks and stylesheet are preloaded and its CSS is applied before it runs.

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
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const PORT = 5511;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-relbase-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.mkdirSync(path.join(app, "nested"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "relbase", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "util.js"), `export const util = "shared-util";\n`);
fs.writeFileSync(path.join(app, "src", "main.css"), `body { background: url(./bg.png) no-repeat; margin: 0 }\n`);
// Larger than assetsInlineLimit (4096), so it is emitted as a file rather than inlined.
fs.writeFileSync(path.join(app, "src", "bg.png"), Buffer.concat([Buffer.from("89504e470d0a1a0a", "hex"), Buffer.alloc(5000, 1)]));
fs.writeFileSync(path.join(app, "src", "lazy.css"), `body { color: rgb(255, 0, 0) }\n`);
fs.writeFileSync(
  path.join(app, "src", "lazy.js"),
  `import "./lazy.css";\nimport { util } from "./util.js";\n` +
    `export const color = getComputedStyle(document.body).color;\nexport const u = util;\n`,
);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import "./main.css";\nwindow.__LAZY_DONE = import("./lazy.js").then((m) => { window.__LAZY = m.color; window.__UTIL = m.u; });\n`,
);
fs.writeFileSync(path.join(app, "src", "nested.js"), `import { util } from "./util.js";\nwindow.__NESTED = util;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "nested", "index.html"),
  `<!doctype html><html><head><title>n</title></head><body><script type="module" src="../src/nested.js"></script></body></html>`,
);

function build(base) {
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  fs.writeFileSync(
    path.join(app, "oj.config.json"),
    JSON.stringify({ base, build: { rollupOptions: { input: ["index.html", "nested/index.html"] } } }),
  );
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const dist = path.join(app, "dist");
  const assets = fs.readdirSync(path.join(dist, "assets"));
  const read = (prefix, ext) => {
    const f = assets.find((a) => a.startsWith(prefix) && a.endsWith(ext));
    assert.ok(f, `${prefix}*${ext} emitted: ${assets.join(",")}`);
    return fs.readFileSync(path.join(dist, "assets", f), "utf8");
  };
  return {
    html: fs.readFileSync(path.join(dist, "index.html"), "utf8"),
    nested: fs.readFileSync(path.join(dist, "nested", "index.html"), "utf8"),
    main: read("main-", ".js"),
    lazy: read("lazy-", ".js"),
    mainCss: read("main-", ".css"),
    assets,
  };
}

let failed = false;
let srv;
try {
  const rel = build("./");
  assert.match(rel.html, /src="\.\/assets\/main-[^"]+\.js"/, "root page script is ./assets/…");
  assert.match(rel.html, /rel="stylesheet" href="\.\/assets\/main-[^"]+\.css"/, "root page css is ./assets/…");
  assert.match(rel.nested, /src="\.\.\/assets\/nested-[^"]+\.js"/, "nested page script is ../assets/…");
  assert.match(rel.nested, /rel="modulepreload" href="\.\.\/assets\/util-[^"]+\.js"/, "nested modulepreload is ../assets/…");
  assert.match(rel.mainCss, /url\("\.\/bg-[a-z0-9]+\.png"\)/, "css url() is a sibling reference");
  assert.match(rel.main, /__vitePreload\(function\(\)\{return import\([`"']\.\/lazy-[^`"']+\.js[`"']\)\},__vite__mapDeps\(\[[\d,]+\]\),import\.meta\.url\)/, `dynamic import wrapped:\n${rel.main.slice(-400)}`);
  assert.match(rel.main, /m\.f=\["\.\/lazy-[^"]+\.js","\.\/lazy-[^"]+\.css","\.\/util-[^"]+\.js"\]/, "deps are chunk-relative: lazy chunk, its css, shared util");
  assert.match(rel.main, /new URL\(dep,importerUrl\)/, "relative base resolves deps against the importer");
  assert.match(rel.main, /vite:preloadError/, "preload helper dispatches vite:preloadError");
  assert.match(rel.main, /relList\.supports/, "page entry carries the modulepreload polyfill");
  assert.doesNotMatch(rel.lazy, /relList\.supports\("modulepreload"\)\)return;for/, "non-entry chunk has no polyfill");

  srv = spawn(oj, ["preview", app, "--port", String(PORT)], { stdio: "ignore" });
  await waitUp(`http://localhost:${PORT}/`);
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  page.on("console", (m) => { if (m.type() === "error" || m.type() === "warning") errors.push(m.type() + ": " + m.text()); });
  try {
    await page.goto(`http://localhost:${PORT}/`, { timeout: 30000 });
    try {
      await page.waitForFunction(() => window.__LAZY !== undefined, { timeout: 15000 });
    } catch (e) {
      const state = await page.evaluate(() => ({
        lazy: window.__LAZY, links: [...document.querySelectorAll("link,script")].map((l) => l.outerHTML),
      }));
      throw new Error(`lazy chunk never ran: ${e.message}\nstate: ${JSON.stringify(state)}\nerrors: ${errors.join(" | ")}`);
    }
    const lazy = await page.evaluate(() => ({ color: window.__LAZY, util: window.__UTIL }));
    assert.equal(lazy.color, "rgb(255, 0, 0)", "lazy chunk css applied before the chunk executed");
    assert.equal(lazy.util, "shared-util", "shared chunk linked");
    await page.goto(`http://localhost:${PORT}/nested/`, { timeout: 30000 });
    await page.waitForFunction(() => window.__NESTED !== undefined, { timeout: 15000 });
    assert.equal(errors.length, 0, `page errors: ${errors.join("|")}`);
  } finally {
    await browser.close();
    srv.kill("SIGKILL");
    srv = null;
    await sleep(200);
  }

  const abs = build("/app/");
  assert.match(abs.html, /src="\/app\/assets\/main-[^"]+\.js"/, "absolute base kept");
  assert.match(abs.main, /m\.f=\["assets\/lazy-[^"]+\.js","assets\/lazy-[^"]+\.css","assets\/util-[^"]+\.js"\]/, "deps are outDir-relative with an absolute base");
  assert.match(abs.main, /function\(dep\)\{return "\/app\/"\+dep\}/, "absolute base is prefixed in the helper");

  // modulePreload: false drops the polyfill.
  fs.writeFileSync(
    path.join(app, "oj.config.json"),
    JSON.stringify({ build: { modulePreload: false, rollupOptions: { input: ["index.html", "nested/index.html"] } } }),
  );
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const mainNoPoly = fs.readdirSync(path.join(app, "dist", "assets")).find((a) => a.startsWith("main-") && a.endsWith(".js"));
  assert.doesNotMatch(fs.readFileSync(path.join(app, "dist", "assets", mainNoPoly), "utf8"), /relList\.supports\("modulepreload"\)\)return;for/, "modulePreload:false removes the polyfill");

  console.log("PRELOAD-RELATIVE-BASE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PRELOAD-RELATIVE-BASE E2E FAILED:", err.stack || err.message);
} finally {
  if (srv) srv.kill("SIGKILL");
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
