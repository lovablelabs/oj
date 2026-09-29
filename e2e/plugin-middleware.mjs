// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

import { spawn, execSync } from "node:child_process";
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

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-mw-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "mw-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `export default [{
    name: "bridge-like",
    configureServer(server) {
      const orig = server.watcher.emit.bind(server.watcher);
      server.watcher.emit = (e, ...a) => orig(e, ...a);
      server.moduleGraph.onFileChange("/x");
      server.ws.on("connection", () => {});
      server.middlewares.use((req, res, next) => {
        if (req.method === "POST" && req.url === "/__plugin/echo") {
          let body = "";
          req.on("data", (c) => (body += c));
          req.on("end", () => {
            res.setHeader("content-type", "application/json");
            res.statusCode = 200;
            res.end(JSON.stringify({ echoed: JSON.parse(body || "{}") }));
          });
          return;
        }
        next();
      });
    },
    transformIndexHtml(html) {
      return html.replace("</head>", '<script>window.__INJECTED = true;</script></head>');
    },
  }];\n`,
);
fs.writeFileSync(path.join(app, "src.js"), `window.__READY = true;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src.js"></script></body></html>`,
);

const port = 5489;
let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${port}/`);

  // POST body forwarded to configureServer middleware, response returned
  const echo = await (
    await fetch(`http://localhost:${port}/__plugin/echo`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ hi: 1, nested: { a: "b" } }),
    })
  ).json();
  assert.deepEqual(echo, { echoed: { hi: 1, nested: { a: "b" } } }, "POST body forwarded to plugin middleware");

  // transformIndexHtml injection applied
  const html = await (await fetch(`http://localhost:${port}/`)).text();
  assert.match(html, /window\.__INJECTED = true/, "transformIndexHtml injection present");

  console.log("PLUGIN-MIDDLEWARE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PLUGIN-MIDDLEWARE E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
