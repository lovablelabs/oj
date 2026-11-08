// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// A GET carrying a body (curl -X GET -d …) must not desync the pooled
// oj -> plugin-host connection: the forward has to carry the same bytes its
// headers promise, so a middleware that answers without reading the body
// leaves no unread bytes behind for the next request to trip over (Node
// would answer that next request 400, empty).

import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");

function raw({ port, method, url, body, headers = {} }) {
  return new Promise((resolve, reject) => {
    const len = body ? Buffer.byteLength(body) : 0;
    const req = http.request(
      {
        host: "127.0.0.1",
        port,
        method,
        path: url,
        headers: { ...headers, ...(body ? { "Content-Length": len } : {}) },
      },
      (res) => {
        const chunks = [];
        res.on("data", (c) => chunks.push(c));
        res.on("end", () =>
          resolve({ status: res.statusCode, body: Buffer.concat(chunks).toString() }),
        );
      },
    );
    req.on("error", reject);
    if (body) req.write(body);
    req.end();
  });
}

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-getbody-"));
fs.writeFileSync(
  path.join(app, "package.json"),
  JSON.stringify({ name: "getbody", version: "1.0.0" }),
);
fs.writeFileSync(
  path.join(app, "index.html"),
  "<!doctype html><html><body>ok</body></html>",
);
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `export default [{
  name: "test:get-body",
  configureServer(server) {
    // Answers without reading the body, like the betaconsulting v9Api().
    server.middlewares.use("/api/", (req, res) => {
      res.setHeader("content-type", "application/json");
      if (req.url.startsWith("/api/me")) { res.statusCode = 200; res.end(JSON.stringify({ ok: true })); return; }
      res.statusCode = 404; res.end(JSON.stringify({ error: "unknown" }));
    });
    // Reads the body: must see the exact bytes, as under Vite.
    server.middlewares.use("/echo", (req, res) => {
      let received = "";
      req.on("data", (c) => (received += c));
      req.on("end", () => {
        res.setHeader("content-type", "application/json");
        res.end(JSON.stringify({ cl: req.headers["content-length"] ?? null, received }));
      });
    });
  },
}];\n`,
);

const port = 5493;
const srv = spawn(oj, ["dev", app, "--port", String(port)], {
  stdio: ["ignore", "inherit", "inherit"],
});
try {
  await waitUp(`http://localhost:${port}/`);

  // 1. GET-with-body reaches the middleware; the pooled connection survives.
  for (let round = 0; round < 4; round++) {
    const g = await raw({
      port,
      method: "GET",
      url: "/api/unknown",
      body: "{}",
      headers: { "Content-Type": "application/json" },
    });
    assert.equal(g.status, 404, `round ${round}: GET-with-body status`);
    assert.match(g.body, /unknown/, `round ${round}: GET-with-body body`);
    const m = await raw({ port, method: "GET", url: "/api/me" });
    assert.equal(m.status, 200, `round ${round}: follow-up GET status`);
    assert.match(m.body, /ok/, `round ${round}: follow-up GET body`);
  }

  // 2. The exact reported curl command, then a follow-up on the pool.
  const bodyFile = path.join(app, "curl-body.txt");
  const curl = spawnSync(
    "curl",
    [
      "-s",
      "-o",
      bodyFile,
      "-w",
      "%{http_code}",
      "-X",
      "GET",
      "-H",
      "Content-Type: application/json",
      "-d",
      "{}",
      `http://127.0.0.1:${port}/api/unknown`,
    ],
    { encoding: "utf8" },
  );
  assert.equal(curl.stdout.trim(), "404", "curl GET-with-body status");
  assert.match(fs.readFileSync(bodyFile, "utf8"), /unknown/, "curl GET-with-body body");
  const after = await raw({ port, method: "GET", url: "/api/me" });
  assert.equal(after.status, 200, "post-curl GET status");

  // 3. A body-reading middleware sees the exact GET bytes, as under Vite.
  const echo = await raw({
    port,
    method: "GET",
    url: "/echo",
    body: '{"a":1}',
    headers: { "Content-Type": "application/json" },
  });
  assert.equal(echo.status, 200, "echo status");
  assert.deepEqual(JSON.parse(echo.body), { cl: "7", received: '{"a":1}' });

  // 4. POST body forwarding intact; static files still served.
  const post = await raw({
    port,
    method: "POST",
    url: "/echo",
    body: '{"b":2}',
    headers: { "Content-Type": "application/json" },
  });
  assert.deepEqual(JSON.parse(post.body), { cl: "7", received: '{"b":2}' });
  const page = await raw({ port, method: "GET", url: "/" });
  assert.equal(page.status, 200, "static page status");
  assert.match(page.body, /ok/, "static page body");

  console.log("plugin-middleware-get-body: ok");
} finally {
  srv.kill("SIGKILL");
}
