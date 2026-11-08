// SPDX-License-Identifier: MIT
// T4 (docs/20261108_native-solid-plugins.md): the app-local `llmsTxt()`
// plugin stays hosted JS — no native port. This pins the parity contract at
// fixture level: a `configureServer` middleware serving `/llms.txt` under OJ
// dev returns the app's rendered content. Mirrors the intersolaris shape:
// `server.middlewares.use("/llms.txt", ...)` ending `renderLlmsTxt()`.
import { test } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { rpcSidecar, tmpProject } from "./harness.mjs";

// Stand-in for the app's `renderLlmsTxt()` (src/lib/llms): rendered from the
// app's own sources in production; here a fixed body proves the middleware
// serves whatever the app renders.
const RENDERED = "# App\n\n> rendered llms.txt body\n\n- [/](https://example.test/)\n";

const PLUGINS = `export function renderLlmsTxt() { return ${JSON.stringify(RENDERED)}; }
export default [
  {
    name: "llms-txt",
    configureServer(server) {
      server.middlewares.use("/llms.txt", (_req, res) => {
        res.setHeader("Content-Type", "text/plain; charset=utf-8");
        res.end(renderLlmsTxt());
      });
    },
  },
];
`;

test("an app-local configureServer plugin serving /llms.txt works under OJ dev", async () => {
  const fx = tmpProject({ prefix: "oj-llms-txt-" });
  fx.write("oj.plugins.mjs", PLUGINS);
  const host = rpcSidecar("plugin-host.mjs", {
    args: [
      path.join(fx.root, "oj.plugins.mjs"),
      JSON.stringify({
        config: { root: fx.root },
        env: { command: "serve", mode: "development" },
        environment: { name: "client" },
      }),
    ],
    env: { OJ_CACHE_ROOT: fx.root },
    cwd: fx.root,
  });
  try {
    const info = JSON.parse((await host.send({ id: 1, hook: "getServeInfo" })).result);
    const port = Number(info.middlewarePort);
    assert.ok(port > 0, "middleware server is up");
    const res = await fetch(`http://127.0.0.1:${port}/llms.txt`);
    assert.equal(res.status, 200, "GET /llms.txt is served");
    assert.match(res.headers.get("content-type") ?? "", /text\/plain/, "content type is text/plain like the app sets");
    assert.equal(await res.text(), RENDERED, "the served body is the app's rendered content");
  } finally {
    host.close();
    fx.cleanup();
  }
});
