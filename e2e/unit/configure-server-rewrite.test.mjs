// SPDX-License-Identifier: MIT

import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

// The plugin host's middleware server, as the Rust side talks to it: a
// request no middleware answers comes back 404 with `x-oj-fallthrough`, and
// `x-oj-rewritten-url` names the url the stack left in `req.url`. Vite runs
// the user's `configureServer` middlewares before `baseMiddleware`
// (server/index.ts), so they see the browser's url with its base, and the
// report has to carry the base too: the Rust side strips it itself, and a
// Start document rendered for a stripped url would miss its route.
async function hostWithBase(base) {
  const fx = tmpProject({ prefix: "oj-rewrite-" });
  fx.write(
    "oj.plugins.mjs",
    `export default [{
       name: "rewrites",
       configureServer(server) {
         server.middlewares.use((req, res, next) => {
           const [p, q] = req.url.split("?");
           const dir = p.slice(0, p.lastIndexOf("/") + 1);
           const name = p.slice(dir.length);
           if (name === "seen") {
             res.setHeader("content-type", "text/plain");
             res.end(req.url);
             return;
           }
           if (name === "clean") req.url = dir + "pages/clean/index.html" + (q ? "?" + q : "");
           next();
         });
       },
     }];\n`,
  );
  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        config: { root: fx.root, ...(base ? { base } : {}) },
        env: { command: "serve", mode: "development" },
        environment: { name: "client" },
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  const info = JSON.parse((await host.send({ id: 1, hook: "getServeInfo" })).result);
  const port = Number(info.middlewarePort);
  assert.ok(port > 0, "middleware server is up");
  const get = (url) => fetch(`http://127.0.0.1:${port}${url}`, { redirect: "manual" });
  return { fx, host, get };
}

test("a fall-through reports the url the middlewares left, rewritten or not", async () => {
  const { fx, host, get } = await hostWithBase(null);
  try {
    const rewritten = await get("/clean?tab=1");
    assert.equal(rewritten.status, 404);
    assert.equal(rewritten.headers.get("x-oj-fallthrough"), "1");
    assert.equal(rewritten.headers.get("x-oj-rewritten-url"), "/pages/clean/index.html?tab=1");

    const untouched = await get("/other?x=1");
    assert.equal(untouched.status, 404);
    assert.equal(untouched.headers.get("x-oj-rewritten-url"), "/other?x=1");
  } finally {
    host.close();
    fx.cleanup();
  }
});

test("under a base, the middlewares see the browser's url and the report keeps the base", async () => {
  const { fx, host, get } = await hostWithBase("/app/");
  try {
    // Vite's baseMiddleware runs after the user's middlewares: they see
    // the url as the browser sent it.
    const seen = await get("/app/x/seen?q=1");
    assert.equal(seen.status, 200);
    assert.equal(await seen.text(), "/app/x/seen?q=1");

    // The rewrite in the browser's form comes back in that form, although
    // the base middleware stripped it for the middlewares after.
    const rewritten = await get("/app/clean?tab=1");
    assert.equal(rewritten.status, 404);
    assert.equal(rewritten.headers.get("x-oj-fallthrough"), "1");
    assert.equal(rewritten.headers.get("x-oj-rewritten-url"), "/app/pages/clean/index.html?tab=1");

    // An untouched url under the base is reported as asked (the Rust side
    // takes a different url as a rewrite), the base itself included.
    const untouched = await get("/app/other?x=1");
    assert.equal(untouched.headers.get("x-oj-rewritten-url"), "/app/other?x=1");
    const root = await get("/app/");
    assert.equal(root.headers.get("x-oj-rewritten-url"), "/app/");

    // A url outside the base passes the base middleware untouched.
    const outside = await get("/elsewhere");
    assert.equal(outside.headers.get("x-oj-rewritten-url"), "/elsewhere");
  } finally {
    host.close();
    fx.cleanup();
  }
});
