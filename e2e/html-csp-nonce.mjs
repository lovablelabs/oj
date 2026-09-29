// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// html.cspNonce (Vite's injectNonceAttributeTagHook): in dev and build every
// script, style and stylesheet/modulepreload link on the page carries the
// nonce, including the tags oj itself injects, plus a csp-nonce meta tag.
// Run with a built target/debug/oj.
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-csp-nonce-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "csp-nonce", type: "module" }));
fs.writeFileSync(path.join(app, "oj.config.ts"), `export default { html: { cspNonce: "abc123" } };\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><link rel="stylesheet" href="/src/page.css"><style>.s{color:red}</style></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);
fs.writeFileSync(path.join(app, "src", "main.js"), `import "./app.css";\nimport("./lazy.js");\n`);
fs.writeFileSync(path.join(app, "src", "lazy.js"), `export const x = 1;\n`);
fs.writeFileSync(path.join(app, "src", "app.css"), `.a { color: blue }\n`);
fs.writeFileSync(path.join(app, "src", "page.css"), `.p { color: green }\n`);

let failed = false;
function check(label, ok, detail) {
  if (!ok) {
    failed = true;
    console.error(`FAIL ${label}: ${detail}`);
  } else {
    console.log(`ok   ${label}`);
  }
}

function everyTagHasNonce(html, re) {
  const tags = html.match(re) ?? [];
  return tags.length > 0 && tags.every((t) => /nonce="abc123"/.test(t));
}

// Dev
{
  const port = 6307;
  const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
  try {
    let html = null;
    await waitUp(`http://localhost:${port}/`, {
      until: async (r) => r.ok && (html = await r.text()) !== undefined,
    }).catch(() => {});
    check("dev server serves the page", !!html, "no response on 6307");
    if (html) {
      check(
        "dev: every script carries the nonce (incl. injected client)",
        everyTagHasNonce(html, /<script[^>]*>/g),
        html,
      );
      check("dev: style carries the nonce", everyTagHasNonce(html, /<style[^>]*>/g), html);
      check(
        "dev: stylesheet link carries the nonce",
        everyTagHasNonce(html, /<link[^>]*rel="stylesheet"[^>]*>/g),
        html,
      );
      check(
        "dev: csp-nonce meta present once",
        (html.match(/property="csp-nonce" nonce="abc123"/g) ?? []).length === 1,
        html,
      );
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
    const html = fs.readFileSync(path.join(app, "dist", "index.html"), "utf8");
    check("build: every script carries the nonce", everyTagHasNonce(html, /<script[^>]*>/g), html);
    check(
      "build: stylesheet and modulepreload links carry the nonce",
      everyTagHasNonce(html, /<link[^>]*rel="(?:stylesheet|modulepreload)"[^>]*>/g),
      html,
    );
    check("build: style carries the nonce", everyTagHasNonce(html, /<style[^>]*>/g), html);
    check(
      "build: csp-nonce meta present once",
      (html.match(/property="csp-nonce" nonce="abc123"/g) ?? []).length === 1,
      html,
    );
  }
}

fs.rmSync(app, { recursive: true, force: true });
if (failed) process.exit(1);
console.log("HTML-CSP-NONCE E2E PASSED");
