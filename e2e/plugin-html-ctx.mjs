// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// transformIndexHtml's ctx is Vite's IndexHtmlTransformContext for the page:
// in dev `{ path, filename, server, originalUrl }` (indexHtml middleware), in a
// build `{ path, filename, bundle, chunk }` (html.ts). A throwing hook fails the
// request (500) or the build, instead of serving the untransformed page.

import { spawn, spawnSync, execSync } from "node:child_process";
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
const PORT = 6410;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-htmlctx-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.mkdirSync(path.join(app, "sub"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "htmlctx", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "main.js"), `document.body.dataset.ok = "1";\n`);
const page = (marker = "") =>
  `<!doctype html><html><head><title>t</title></head><body>${marker}<script type="module" src="/src/main.js"></script></body></html>`;
fs.writeFileSync(path.join(app, "index.html"), page());
fs.writeFileSync(path.join(app, "sub", "page.html"), page());
fs.writeFileSync(path.join(app, "throw.html"), page("<!-- THROW -->"));
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `export default [{
  name: "html-ctx",
  transformIndexHtml(html, ctx) {
    if (html.includes("THROW") || process.env.OJ_TEST_HTML_THROW === "1") throw new Error("no html for you");
    const content = [
      ctx.path,
      ctx.filename,
      ctx.server ? "server" : "-",
      ctx.bundle ? Object.keys(ctx.bundle).length : "-",
      ctx.chunk ? ctx.chunk.fileName : "-",
    ].join("|");
    return [{ tag: "meta", attrs: { name: "ctx", content } }];
  },
}];\n`,
);

const meta = (html) => {
  const m = /<meta name="ctx" content="([^"]*)"/.exec(html);
  return m ? m[1].split("|") : null;
};

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${PORT}/`);

  const root = await fetch(`http://localhost:${PORT}/`);
  assert.equal(root.status, 200);
  const rootCtx = meta(await root.text());
  assert.ok(rootCtx, "the root page was transformed");
  assert.equal(rootCtx[0], "/index.html", "dev ctx.path is the page url");
  assert.equal(rootCtx[1], path.join(fs.realpathSync(app), "index.html"), "dev ctx.filename is the html on disk");
  assert.equal(rootCtx[2], "server", "dev ctx carries the server");

  const sub = await fetch(`http://localhost:${PORT}/sub/page.html`);
  assert.equal(sub.status, 200);
  const subCtx = meta(await sub.text());
  assert.equal(subCtx[0], "/sub/page.html", "a nested page reports its own path");
  assert.equal(subCtx[1], path.join(fs.realpathSync(app), "sub", "page.html"));

  const bad = await fetch(`http://localhost:${PORT}/throw.html`);
  assert.equal(bad.status, 500, "a throwing transformIndexHtml fails the request");
  const body = await bad.text();
  assert.match(body, /\[plugin:html-ctx\] no html for you/, `the response names the plugin:\n${body}`);
  assert.ok(!body.includes("<!-- THROW -->"), "the raw page is not served");
  srv.kill("SIGKILL");
  await sleep(200);

  // Build: ctx carries the output bundle and the page's entry chunk.
  fs.rmSync(path.join(app, "throw.html"));
  const build = spawnSync(oj, ["build", app], { encoding: "utf8" });
  assert.equal(build.status, 0, `build succeeds:\n${build.stdout}\n${build.stderr}`);
  const built = meta(fs.readFileSync(path.join(app, "dist", "index.html"), "utf8"));
  assert.ok(built, "the built page was transformed");
  assert.equal(built[0], "/index.html", "build ctx.path");
  assert.equal(built[1], path.join(fs.realpathSync(app), "index.html"), "build ctx.filename is the source html");
  assert.ok(Number(built[3]) >= 1, `build ctx.bundle lists the output: ${built}`);
  assert.match(built[4], /^assets\/.*\.js$/, `build ctx.chunk is the page's entry chunk: ${built}`);

  const failing = spawnSync(oj, ["build", app], { encoding: "utf8", env: { ...process.env, OJ_TEST_HTML_THROW: "1" } });
  assert.notEqual(failing.status, 0, "a throwing transformIndexHtml fails the build");
  assert.match(failing.stderr + failing.stdout, /\[plugin:html-ctx\] no html for you/, `the build error names the plugin:\n${failing.stderr}`);
  console.log("PLUGIN-HTML-CTX E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PLUGIN-HTML-CTX E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
