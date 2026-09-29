// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// A vite.config `oxc.jsx.importSource` (what `@vitejs/plugin-react`'s
// `jsxImportSource` becomes, e.g. for Emotion) must drive the JSX runtime import
// in dev and in the build, and a file's own `@jsxImportSource` pragma must still
// win. The fake packages under node_modules stand in for the real runtimes so
// resolution succeeds without installing anything.

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
const PORT = 5483;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-jsx-src-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "jsx-src", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "vite.config.js"),
  `export default { oxc: { jsx: { runtime: "automatic", importSource: "@fake/emotion" } } };\n`,
);
function fakeRuntime(name) {
  const dir = path.join(app, "node_modules", ...name.split("/"));
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(
    path.join(dir, "package.json"),
    JSON.stringify({
      name,
      version: "1.0.0",
      exports: { "./jsx-runtime": "./jsx-runtime.js", "./jsx-dev-runtime": "./jsx-dev-runtime.js" },
    }),
  );
  const body = `export const Fragment = Symbol("${name}");\nexport const jsx = (t, p) => ({ from: "${name}", t, p });\n`;
  fs.writeFileSync(path.join(dir, "jsx-runtime.js"), body + `export const jsxs = jsx;\n`);
  fs.writeFileSync(path.join(dir, "jsx-dev-runtime.js"), body + `export const jsxDEV = jsx;\n`);
}
fakeRuntime("@fake/emotion");
fakeRuntime("@fake/solid");
fs.writeFileSync(path.join(app, "src", "App.jsx"), `export const App = () => <div>hi</div>;\n`);
fs.writeFileSync(
  path.join(app, "src", "Pragma.jsx"),
  `/** @jsxImportSource @fake/solid */\nexport const Pragma = () => <span>p</span>;\n`,
);
fs.writeFileSync(
  path.join(app, "src", "main.js"),
  `import { App } from "./App";\nimport { Pragma } from "./Pragma";\nwindow.__APP = App().from; window.__PRAGMA = Pragma().from;\n`,
);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${PORT}/`);
  const appJs = await (await fetch(`http://localhost:${PORT}/src/App.jsx`)).text();
  assert.match(
    appJs,
    /@fake\/emotion\/jsx-dev-runtime/,
    `dev App.jsx does not import the configured source:\n${appJs}`,
  );
  assert.doesNotMatch(appJs, /["/]react\/jsx/, "dev App.jsx still imports react's runtime");
  const pragmaJs = await (await fetch(`http://localhost:${PORT}/src/Pragma.jsx`)).text();
  assert.match(pragmaJs, /@fake\/solid\/jsx-dev-runtime/, `dev pragma did not win:\n${pragmaJs}`);
  srv.kill("SIGKILL");
  await sleep(300);

  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  execSync(`${oj} build ${app}`, { stdio: "ignore" });
  const assets = path.join(app, "dist", "assets");
  const built = fs
    .readdirSync(assets)
    .filter((f) => f.endsWith(".js"))
    .map((f) => fs.readFileSync(path.join(assets, f), "utf8"))
    .join("\n");
  assert.match(built, /@fake\/emotion/, "build did not use the configured importSource");
  assert.match(built, /@fake\/solid/, "build did not honor the file pragma");
  assert.doesNotMatch(built, /react\/jsx-runtime/, "build still references react's runtime");
  console.log("JSX-IMPORT-SOURCE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("JSX-IMPORT-SOURCE E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(200);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
