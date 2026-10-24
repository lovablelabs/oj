// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// The Start rebundle gate: a change whose paths miss every served graph (the
// client bundle's input closure, the engine's and the workers' module
// graphs) must not rebundle the client or reload the browser -- Vite does
// nothing for a file its graphs never served. A change to a bundled module
// must still rebundle and serve fresh. Run with a built target/debug/oj (or
// OJ_BIN).
import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = 6861;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const installed = fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start"));
if (!installed) {
  console.log("SKIP start rebundle gate: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-gate-"));
const app = path.join(tmp, "app");
fs.mkdirSync(app);
for (const f of ["src", "public", "styles", "packages", "tsconfig.json", "package.json", "vite.config.ts"]) {
  fs.cpSync(path.join(fixture, f), path.join(app, f), { recursive: true });
}
fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");
// Present before boot, so its edit below is an update of a known file: the
// scenario pins the update path of the gate (the create path has its own
// scenarios below).
fs.writeFileSync(path.join(app, "README.md"), "# fixture\n");
// The index route's glob reaches into subdirectories, so a directory moved
// under it adds members too.
const indexRoute = path.join(app, "src", "routes", "index.tsx");
fs.writeFileSync(
  indexRoute,
  fs
    .readFileSync(indexRoute, "utf8")
    .replace('import.meta.glob("../content/*.json"', 'import.meta.glob("../content/**/*.json"'),
);

let log = "";
const boot = () => {
  const proc = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
  proc.stdout.on("data", (c) => (log += c));
  proc.stderr.on("data", (c) => (log += c));
  return proc;
};
let srv = boot();

const get = async (route) => {
  const res = await fetch(`http://localhost:${PORT}${route}`);
  return { status: res.status, body: await res.text() };
};

let failed = false;
try {
  await waitUp(`http://localhost:${PORT}/`, { proc: srv });
  const about = await get("/about");
  if (about.status !== 200 || !about.body.includes("about-page-marker")) {
    throw new Error(`/about did not render (${about.status})`);
  }

  // A change outside every served graph: no rebundle, no reload.
  const bundledBefore = log.split("client bundled").length;
  fs.appendFileSync(path.join(app, "README.md"), "\nan edit outside every graph\n");
  await settles(async () => log.includes("change outside every served graph"), { timeoutMs: 10000 });
  await sleep(1500);
  const bundledAfter = log.split("client bundled").length;
  if (bundledAfter !== bundledBefore) {
    throw new Error(
      `README edit rebundled the client (${bundledAfter - bundledBefore} run(s)); log tail:\n${log.slice(-3000)}`,
    );
  }
  console.log("gate: README edit skipped the rebundle and the reload");

  // Files CREATED outside every graph and glob scope: miniflare persists
  // under `.wrangler` (trace-store sqlite with workers observability on,
  // D1/KV state, tmp bundles), creating journal files as it goes. When any
  // create forced a rebundle, each journal create ran a full rebundle whose
  // workerd restart wrote the store again -- a self-sustaining loop pumping
  // gigabytes. Vite ignores adds its module graphs and glob importers never
  // reach, and so must the gate. The dir is born after boot, as miniflare
  // creates it, covering the new-top-level-entry adoption path too.
  const wranglerBefore = log.split("client bundled").length;
  const traceDir = path.join(app, ".wrangler", "state", "v3", "observability", "trace-store");
  fs.mkdirSync(traceDir, { recursive: true });
  for (let i = 0; i < 3; i++) {
    fs.writeFileSync(path.join(traceDir, `store.sqlite-${i}-wal`), `wal ${i}\n`);
    fs.appendFileSync(path.join(traceDir, "store.sqlite"), `trace ${i}\n`);
    await sleep(200);
  }
  await sleep(1800);
  if (log.split("client bundled").length !== wranglerBefore) {
    throw new Error(`.wrangler state writes rebundled the client; log tail:\n${log.slice(-3000)}`);
  }
  if (/rebuilding for .*\.wrangler/.test(log)) {
    throw new Error(`.wrangler writes reached the rebuild pipeline; log tail:\n${log.slice(-3000)}`);
  }
  console.log("gate: .wrangler state writes never reached the watcher");

  // A file created under a bundled import.meta.glob scan base still
  // rebundles: the glob's expansion grows, as when Vite re-expands glob
  // importers on add. The index route renders jsglob:<titles> from
  // src/content/*.json (glob-plain.js), so the new title must be served.
  const globBefore = log.split("client bundled").length;
  fs.writeFileSync(path.join(app, "src", "content", "gate-extra.json"), JSON.stringify({ title: "gate-extra-title" }));
  await settles(async () => log.split("client bundled").length > globBefore, { timeoutMs: 20000 });
  if (log.split("client bundled").length === globBefore) {
    throw new Error(`a create under a glob scope did not rebundle; log tail:\n${log.slice(-3000)}`);
  }
  // The fresh bundle carries the new entry (the eager glob inlines the json).
  // The SSR document's freshness is the engine's own path, as above.
  const chunksHold = (needle) => {
    const dir = path.join(app, ".oj-cache");
    let files;
    try {
      files = fs.readdirSync(dir, { recursive: true });
    } catch {
      // The bundle store renames its temp dir into place mid-walk; the
      // next probe sees the settled tree.
      return false;
    }
    return files
      .filter((f) => String(f).endsWith(".js"))
      .some((f) => {
        try {
          return fs.readFileSync(path.join(dir, String(f)), "utf8").includes(needle);
        } catch {
          return false;
        }
      });
  };
  await settles(async () => chunksHold("gate-extra-title"), { timeoutMs: 15000 });
  if (!chunksHold("gate-extra-title")) {
    throw new Error("the rebundled client does not carry the new glob entry");
  }
  console.log("gate: create under a glob scope rebundled with the new entry");

  // A file renamed into place (an atomic write) is a rename event, not a
  // create, and a directory moved into place is one event for the directory.
  // Chokidar calls both adds (a path it did not know), so Vite re-expands the
  // glob for them as for any create.
  const staged = path.join(app, "src", ".gate-renamed.json.tmp");
  fs.writeFileSync(staged, JSON.stringify({ title: "gate-renamed-title" }));
  fs.renameSync(staged, path.join(app, "src", "content", "gate-renamed.json"));
  await settles(async () => chunksHold("gate-renamed-title"), { timeoutMs: 20000 });
  if (!chunksHold("gate-renamed-title")) {
    throw new Error(`a file renamed under a glob scope did not rebundle; log tail:\n${log.slice(-3000)}`);
  }
  console.log("gate: a file renamed under a glob scope rebundled with the new entry");
  const stagedDir = path.join(tmp, "staged-dir");
  fs.mkdirSync(path.join(stagedDir, "nested"), { recursive: true });
  fs.writeFileSync(path.join(stagedDir, "nested", "deep.json"), JSON.stringify({ title: "gate-moved-dir-title" }));
  fs.renameSync(stagedDir, path.join(app, "src", "content", "gate-moved"));
  await settles(async () => chunksHold("gate-moved-dir-title"), { timeoutMs: 20000 });
  if (!chunksHold("gate-moved-dir-title")) {
    throw new Error(`a directory moved under a glob scope did not rebundle; log tail:\n${log.slice(-3000)}`);
  }
  console.log("gate: a directory moved under a glob scope rebundled with its members");

  // A bundled module still rebundles and reloads. (The served document's
  // freshness is the engine's own path and not asserted here: the base
  // behavior predates the gate and is unchanged by it.)
  const aboutFile = path.join(app, "src", "routes", "about.tsx");
  const editBefore = log.split("client bundled").length;
  const reloadBefore = log.split("rebuilt, reloading").length;
  fs.writeFileSync(aboutFile, fs.readFileSync(aboutFile, "utf8").replace("about-page-marker", "about-page-edited"));
  await settles(async () => log.split("client bundled").length > editBefore, { timeoutMs: 20000 });
  if (log.split("client bundled").length === editBefore) {
    throw new Error(`route edit did not rebundle; log tail:\n${log.slice(-3000)}`);
  }
  await settles(async () => log.split("rebuilt, reloading").length > reloadBefore, { timeoutMs: 10000 });
  if (log.split("rebuilt, reloading").length === reloadBefore) {
    throw new Error(`route edit did not reload; log tail:\n${log.slice(-3000)}`);
  }
  console.log("gate: bundled-module edit rebundled and reloaded");

  // A glob member added while no server ran is in no stored closure, so the
  // client bundle cache must not restore the bundle that lacks it: Vite
  // expands every glob anew on each start.
  srv.kill("SIGTERM");
  await new Promise((r) => srv.once("exit", r));
  fs.writeFileSync(
    path.join(app, "src", "content", "gate-offline.json"),
    JSON.stringify({ title: "gate-offline-title" }),
  );
  srv = boot();
  await waitUp(`http://localhost:${PORT}/`, { proc: srv });
  const served = async () => {
    const html = (await get("/")).body;
    let code = "";
    for (const [, src] of html.matchAll(/src="([^"]+\.js[^"]*)"/g)) code += (await get(src)).body;
    for (const [, src] of code.matchAll(/["'](\/[^"']+\.js)["']/g)) code += (await get(src)).body;
    return code;
  };
  if (!(await served()).includes("gate-offline-title")) {
    throw new Error(
      `a glob member added between runs is missing from the served bundle; log tail:\n${log.slice(-3000)}`,
    );
  }
  if (!log.includes("is new to a bundled glob")) {
    throw new Error(`the restart did not miss the client bundle cache; log tail:\n${log.slice(-3000)}`);
  }
  console.log("gate: a glob member added between runs missed the cache and is served");
  console.log("START-REBUNDLE-GATE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("START-REBUNDLE-GATE E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  for (let i = 0; ; i++) {
    try {
      fs.rmSync(tmp, { recursive: true, force: true });
      break;
    } catch (e) {
      if (i >= 20) break;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100);
    }
  }
}
process.exit(failed ? 1 : 0);
