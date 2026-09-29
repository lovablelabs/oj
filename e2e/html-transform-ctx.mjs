// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// transformIndexHtml receives Vite's per-page context: `path` is the page's
// root-relative url and `filename` its source file, for every page in dev and
// in a multi-page build. Run with a built target/debug/oj.
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-html-ctx-"));
fs.mkdirSync(path.join(app, "nested"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "html-ctx", type: "module" }));
fs.writeFileSync(
  path.join(app, "vite.config.mjs"),
  `import { basename } from "node:path";
export default {
  build: { rollupOptions: { input: { main: "index.html", about: "nested/about.html" } } },
  plugins: [
    {
      name: "page-stamp",
      transformIndexHtml(html, ctx) {
        return html.replace("<head>", \`<head><meta name="page" content="\${ctx.path}|\${basename(ctx.filename)}">\`);
      },
    },
  ],
};
`,
);
fs.writeFileSync(path.join(app, "main.js"), `document.body.textContent = "main";\n`);
fs.writeFileSync(path.join(app, "nested", "about.js"), `document.body.textContent = "about";\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>main</title></head><body><script type="module" src="/main.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "nested", "about.html"),
  `<!doctype html><html><head><title>about</title></head><body><script type="module" src="./about.js"></script></body></html>`,
);

let failed = false;
function check(label, ok, detail) {
  if (!ok) {
    failed = true;
    console.error(`FAIL ${label}: ${detail}`);
  } else {
    console.log(`ok   ${label}`);
  }
}

// Dev
{
  const port = 6308;
  const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
  try {
    let up = false;
    up = await waitUp(`http://localhost:${port}/`).then(() => true, () => false);
    check("dev server starts", up, "no response on 6308");
    if (up) {
      const main = await (await fetch(`http://localhost:${port}/`)).text();
      check("dev: root page gets /index.html ctx", /content="\/index\.html\|index\.html"/.test(main), main);
      const about = await (await fetch(`http://localhost:${port}/nested/about.html`)).text();
      check("dev: nested page gets its own ctx", /content="\/nested\/about\.html\|about\.html"/.test(about), about);
    }
  } finally {
    srv.kill();
  }
}

// Build
{
  const r = spawnSync(oj, ["build", app], { cwd: repo, encoding: "utf8" });
  check("build succeeds", r.status === 0, `${r.stdout}\n${r.stderr}`);
  if (r.status === 0) {
    const main = fs.readFileSync(path.join(app, "dist", "index.html"), "utf8");
    check("build: root page gets /index.html ctx", /content="\/index\.html\|index\.html"/.test(main), main);
    const about = fs.readFileSync(path.join(app, "dist", "nested", "about.html"), "utf8");
    check("build: nested page gets its own ctx", /content="\/nested\/about\.html\|about\.html"/.test(about), about);
  }
}

fs.rmSync(app, { recursive: true, force: true });
if (failed) process.exit(1);
console.log("HTML-TRANSFORM-CTX E2E PASSED");
