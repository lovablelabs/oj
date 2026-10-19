// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// `oj build --watch`: a full rebuild on every debounced file change. Vite
// rebuilds through rolldown's native watcher; oj watches the project with
// notify and re-runs its one-shot build. This drives the real binary through
// a source edit, a CSS edit, an HTML edit, ignores writes under outDir, and
// stops on SIGTERM.

import { execSync, spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitFor(pred, what, ms = 40000) {
  const start = Date.now();
  while (Date.now() - start < ms) {
    if (pred()) return;
    await sleep(50);
  }
  throw new Error(`timeout waiting for ${what}`);
}
const once = (emitter, event) => new Promise((r) => emitter.once(event, (...args) => r(args)));

function readMain(dir) {
  const assets = path.join(dir, "assets");
  const js = fs.existsSync(assets)
    ? fs.readdirSync(assets).find((f) => f.startsWith("main-") && f.endsWith(".js"))
    : undefined;
  return js ? fs.readFileSync(path.join(assets, js), "utf8") : "";
}
function readMainCss(dir) {
  const assets = path.join(dir, "assets");
  const css = fs.existsSync(assets)
    ? fs.readdirSync(assets).find((f) => f.startsWith("main-") && f.endsWith(".css"))
    : undefined;
  return css ? fs.readFileSync(path.join(assets, css), "utf8") : "";
}
function rebuilds(log) {
  return (log().match(/oj build:/g) || []).length;
}

const base = fs.mkdtempSync(path.join(os.tmpdir(), "oj-buildwatch-"));
const app = path.join(base, "app");
const dist = path.join(app, "dist");
const logPath = path.join(base, "watch.log");
let watch;

try {
  fs.mkdirSync(path.join(app, "src"), { recursive: true });
  fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "watch", version: "1.0.0", type: "module" }));
  fs.writeFileSync(
    path.join(app, "index.html"),
    `<!doctype html><html><head><title>watch-one</title><link rel="stylesheet" href="/src/style.css"></head><body><div id="root"></div><script type="module" src="/src/main.js"></script></body></html>`,
  );
  fs.writeFileSync(
    path.join(app, "src", "main.js"),
    `import "./style.css";\ndocument.getElementById("root").textContent = "WATCH_ONE";\n`,
  );
  fs.writeFileSync(path.join(app, "src", "style.css"), `.a {\n  color: red;\n}\n`);

  fs.writeFileSync(logPath, "");
  const logFd = fs.openSync(logPath, "w");
  // The log lives outside the app: a file written into the watched root would
  // itself trigger rebuilds (as any build output would).
  watch = spawn(oj, ["build", app, "--watch"], { stdio: ["ignore", logFd, logFd] });
  const readLog = () => fs.readFileSync(logPath, "utf8");

  await waitFor(() => readLog().includes("watching for file changes"), "watch banner");
  await waitFor(() => readMain(dist).includes("WATCH_ONE"), "initial bundle");
  console.log("ok: --watch builds once, then watches");

  // A source edit rebuilds the bundle.
  fs.writeFileSync(
    path.join(app, "src", "main.js"),
    `import "./style.css";\ndocument.getElementById("root").textContent = "WATCH_TWO";\n`,
  );
  await waitFor(() => readMain(dist).includes("WATCH_TWO"), "rebuilt js after an edit");
  console.log("ok: a source edit rebuilds");

  // A CSS edit rebuilds the stylesheet (a hex value, not a named color: a CSS
  // minifier is free to rewrite names).
  fs.writeFileSync(path.join(app, "src", "style.css"), `.a {\n  color: #123456;\n}\n`);
  await waitFor(() => readMainCss(dist).includes("123456"), "rebuilt css after an edit");
  console.log("ok: a css edit rebuilds");

  // An HTML edit re-emits the page.
  let html = fs.readFileSync(path.join(app, "index.html"), "utf8").replace("watch-one", "watch-two");
  fs.writeFileSync(path.join(app, "index.html"), html);
  await waitFor(() => fs.readFileSync(path.join(dist, "index.html"), "utf8").includes("watch-two"), "re-emitted html");
  console.log("ok: an html edit re-emits the page");

  // Writes under outDir are ignored: no rebuild loop.
  await sleep(300);
  const before = rebuilds(readLog);
  fs.writeFileSync(path.join(dist, "stray.txt"), "not a source");
  fs.mkdirSync(path.join(dist, "sub"), { recursive: true });
  fs.writeFileSync(path.join(dist, "sub", "stray2.txt"), "not a source");
  await sleep(1500);
  assert.equal(rebuilds(readLog), before, `outDir writes must not trigger rebuilds:\n${readLog()}`);
  console.log("ok: outDir writes do not trigger a rebuild");

  // SIGTERM ends the watcher (no signal handler: the process is terminated).
  watch.kill("SIGTERM");
  const [code, signal] = await once(watch, "exit");
  assert.ok(code === 0 || signal === "SIGTERM", `expected a clean stop, got code=${code} signal=${signal}`);
  watch = undefined;
  console.log("ok: SIGTERM stops the watcher");
} catch (e) {
  console.error("FAIL:", e.message);
  if (fs.existsSync(logPath)) console.error("watch log:\n" + fs.readFileSync(logPath, "utf8").slice(-4000));
  process.exitCode = 1;
} finally {
  if (watch) watch.kill("SIGKILL");
  fs.rmSync(base, { recursive: true, force: true });
}
