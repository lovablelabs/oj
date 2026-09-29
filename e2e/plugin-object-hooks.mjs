// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite unwraps every hook's object form `{ order, filter, handler }` and sorts a
// hook's plugins by `order`; a string id filter is a glob joined to the root;
// `configEnvironment` runs per environment; `hotUpdate` (or `handleHotUpdate`)
// receives `server`. Each of those used to be silently skipped or mis-matched.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const PORT = 5500;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-objhooks-"));
fs.mkdirSync(path.join(app, "src", "deep"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "objhooks", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "deep", "a.js"), `export const A = "a";\n`);
fs.writeFileSync(path.join(app, "other.js"), `export const O = "o";\n`);
fs.writeFileSync(path.join(app, "src", "main.js"), `import "./deep/a.js";\nimport "../other.js";\nwindow.__ok = 1;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);
// Outside the app root so the watcher never sees the mark files themselves.
const marks = fs.mkdtempSync(path.join(os.tmpdir(), "oj-objhooks-marks-"));
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `import fs from "node:fs";
const mark = (n, v = "1") => fs.writeFileSync(${JSON.stringify(marks)} + "/" + n, String(v));
export default [
  {
    name: "second",
    // order:"post" on a config hook must run after the "first" plugin's despite array position.
    config: { order: "post", handler(c) { mark("config-order", (fs.existsSync(${JSON.stringify(marks)} + "/config-first") ? "first-then-second" : "second-first")); return { define: { __SECOND__: "2" } }; } },
  },
  {
    name: "first",
    config: { handler() { mark("config-first"); return { define: { __FIRST__: "1" } }; } },
    configResolved: { handler(rc) { mark("configResolved", JSON.stringify({ first: rc.define.__FIRST__, second: rc.define.__SECOND__ })); } },
    configEnvironment: { handler(name, opts) { mark("env-" + name, JSON.stringify(opts)); return { define: { __ENV__: JSON.stringify(name) } }; } },
    configureServer: { handler(server) { server.middlewares.use("/__objhook", (req, res) => { res.setHeader("content-type", "text/plain"); res.end("middleware-ok"); }); } },
    // Glob id filter joined to the root: matches src/deep/a.js but not other.js.
    // A side effect (not a comment) so the marker survives the build's minifier.
    transform: { filter: { id: "src/**/*.js" }, handler(code, id) { return code + "\\nglobalThis.__T = \\"TRANSFORMED " + id.replace(/\\\\/g, "/").split("/").pop() + "\\";\\n"; } },
    hotUpdate: { handler(ctx) { mark("hotUpdate", JSON.stringify({ file: ctx.file.split("/").pop(), hasServer: !!(ctx.server && ctx.server.ws && typeof ctx.server.ws.send === "function"), type: ctx.type })); } },
    buildStart: { handler() { mark("buildStart"); } },
  },
];
`,
);

const read = (n) => fs.readFileSync(path.join(marks, n), "utf8");
let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${PORT}/`);

  assert.equal(read("config-order"), "first-then-second", "config order:post ran before the normal hook");
  assert.deepEqual(JSON.parse(read("configResolved")), { first: "1", second: "2" }, "configResolved object form saw both config results");
  assert.ok(fs.existsSync(path.join(marks, "env-client")) && fs.existsSync(path.join(marks, "env-ssr")), "configEnvironment ran per environment");

  const mw = await fetch(`http://localhost:${PORT}/__objhook`);
  assert.equal(await mw.text(), "middleware-ok", "configureServer object form registered its middleware");

  const a = await (await fetch(`http://localhost:${PORT}/src/deep/a.js`)).text();
  assert.match(a, /TRANSFORMED a\.js/, "glob id filter should match src/deep/a.js");
  const other = await (await fetch(`http://localhost:${PORT}/other.js`)).text();
  assert.doesNotMatch(other, /TRANSFORMED/, "glob id filter must not match a file outside src/");

  fs.writeFileSync(path.join(app, "src", "deep", "a.js"), `export const A = "a2";\n`);
  await settles(() => fs.existsSync(path.join(marks, "hotUpdate")));
  const hot = JSON.parse(read("hotUpdate"));
  assert.equal(hot.file, "a.js", "hotUpdate object form was dispatched");
  assert.equal(hot.hasServer, true, "hotUpdate context carries server.ws.send");
  srv.kill("SIGKILL");
  await sleep(300);

  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  assert.ok(fs.existsSync(path.join(marks, "buildStart")), "buildStart object form ran in the build");
  const built = fs.readdirSync(path.join(app, "dist", "assets")).map((f) => fs.readFileSync(path.join(app, "dist", "assets", f), "utf8")).join("\n");
  assert.match(built, /TRANSFORMED a\.js/, "build applied the glob-filtered transform");
  console.log("PLUGIN-OBJECT-HOOKS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PLUGIN-OBJECT-HOOKS E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
  fs.rmSync(marks, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
