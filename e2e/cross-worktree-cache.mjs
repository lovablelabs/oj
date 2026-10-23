// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// pulp P1 capstone: two worktrees with identical content share one module
// store. Worktree A compiles and populates the shared store; worktree B, a
// fresh directory that has never built anything, must serve byte-identical
// modules WITHOUT recompiling (the store's entries stay untouched), and
// nothing served may contain either worktree's absolute path.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

// One isolated shared store for this test run.
const store = fs.mkdtempSync(path.join(os.tmpdir(), "oj-xwt-store-"));
const modulesDir = path.join(store, "v1", "modules");

function makeApp(label) {
  const app = fs.mkdtempSync(path.join(os.tmpdir(), `oj-xwt-${label}-`));
  fs.mkdirSync(path.join(app, "src"), { recursive: true });
  fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "xwt", version: "1.0.0" }));
  fs.writeFileSync(path.join(app, "package-lock.json"), JSON.stringify({ lockfileVersion: 3 }));
  fs.writeFileSync(
    path.join(app, "index.html"),
    `<!doctype html><html><head><title>x</title></head><body><div id="root"></div><script type="module" src="/main.tsx"></script></body></html>`,
  );
  fs.writeFileSync(path.join(app, "main.tsx"), `import { App } from "./src/App.tsx";\nconsole.log(App);\n`);
  // JSX so the dev __source/sourcemap path-independence is exercised. The
  // automatic runtime needs a resolvable react: a tiny stub is enough (and
  // identical in both worktrees, like a real install would be).
  fs.writeFileSync(
    path.join(app, "src", "App.tsx"),
    `export function App() {\n  return <div className="x">hello</div>;\n}\n`,
  );
  const react = path.join(app, "node_modules", "react");
  fs.mkdirSync(react, { recursive: true });
  fs.writeFileSync(
    path.join(react, "package.json"),
    JSON.stringify({
      name: "react",
      version: "19.0.0",
      exports: {
        ".": "./index.js",
        "./jsx-runtime": "./jsx-runtime.js",
        "./jsx-dev-runtime": "./jsx-dev-runtime.js",
      },
    }),
  );
  fs.writeFileSync(path.join(react, "index.js"), "export default {};\n");
  const rt =
    "export const Fragment = Symbol.for('react.fragment');\nexport function jsxDEV(t, p) { return { t, p }; }\nexport const jsx = jsxDEV;\nexport const jsxs = jsxDEV;\n";
  fs.writeFileSync(path.join(react, "jsx-runtime.js"), rt);
  fs.writeFileSync(path.join(react, "jsx-dev-runtime.js"), rt);
  return app;
}

function listModuleEntries() {
  const out = new Map();
  let shards;
  try {
    shards = fs.readdirSync(modulesDir);
  } catch {
    return out;
  }
  for (const shard of shards) {
    const dir = path.join(modulesDir, shard);
    if (!/^[0-9a-f]{2}$/.test(shard) || !fs.statSync(dir).isDirectory()) continue;
    for (const f of fs.readdirSync(dir)) {
      if (f.endsWith(".json")) out.set(`${shard}/${f}`, fs.statSync(path.join(dir, f)).mtimeMs);
    }
  }
  return out;
}

function localModuleEntries(app) {
  let n = 0;
  const root = path.join(app, ".oj-cache");
  const walk = (dir) => {
    let ents;
    try {
      ents = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const e of ents) {
      const p = path.join(dir, e.name);
      if (e.isDirectory()) walk(p);
      else if (e.isFile() && e.name.endsWith(".json") && /^[0-9a-f]{2}$/.test(path.basename(dir))) n++;
    }
  };
  walk(root);
  return n;
}

async function serveAndFetch(app, port) {
  const log = path.join(app, "server.log");
  const logFd = fs.openSync(log, "w");
  const proc = spawn(oj, ["dev", app, "--port", String(port), "--enable-cache"], {
    stdio: ["ignore", logFd, logFd],
    env: { ...process.env, OJ_STORE_DIR: store },
  });
  try {
    let up = false;
    for (let i = 0; i < 120 && !up; i++) {
      try {
        up = (await fetch(`http://localhost:${port}/`)).ok;
      } catch {}
      if (!up) await sleep(100);
    }
    assert.ok(up, "oj never came up");
    const bodies = {};
    for (const url of ["/main.tsx", "/src/App.tsx"]) {
      const r = await fetch(`http://localhost:${port}${url}`);
      if (r.status !== 200) console.error(fs.readFileSync(log, "utf8"));
      assert.equal(r.status, 200, `${url} serves`);
      bodies[url] = await r.text();
    }
    // Cache writes are async; give the writer a moment.
    for (let i = 0; i < 30 && listModuleEntries().size < 2; i++) await sleep(150);
    await sleep(300);
    return bodies;
  } finally {
    try {
      execSync(`pkill -P ${proc.pid}`);
    } catch {}
    try {
      proc.kill("SIGKILL");
    } catch {}
    try {
      execSync(`lsof -ti:${port} -sTCP:LISTEN | xargs -r kill -9`);
    } catch {}
    await sleep(500);
  }
}

let failed = false;
try {
  const appA = makeApp("a");
  const appB = makeApp("b");

  // Worktree A compiles and populates the shared store.
  const bodiesA = await serveAndFetch(appA, 5317);
  const afterA = listModuleEntries();
  assert.ok(afterA.size >= 2, `worktree A must populate the shared store (got ${afterA.size} entries)`);

  // Worktree B has never built anything.
  const bodiesB = await serveAndFetch(appB, 5318);
  const afterB = listModuleEntries();

  for (const url of ["/main.tsx", "/src/App.tsx"]) {
    assert.equal(bodiesB[url], bodiesA[url], `${url}: worktree B must serve worktree A's exact bytes`);
    assert.ok(
      !bodiesB[url].includes(appA) && !bodiesB[url].includes(appB),
      `${url}: served module must contain no worktree path`,
    );
  }
  assert.ok(bodiesB["/src/App.tsx"].includes("/src/App.tsx"), "jsx __source / map name the module by its served path");

  // The store proves B hit instead of recompiling: no new entries, and no
  // entry rewritten (identical mtimes).
  assert.equal(afterB.size, afterA.size, "worktree B must add no module entries");
  for (const [name, mtime] of afterA) {
    assert.equal(afterB.get(name), mtime, `entry ${name} must not be rewritten by a hit`);
  }
  // And nothing landed in B's per-app cache: every entry was relocatable.
  assert.equal(localModuleEntries(appB), 0, "no pinned entries for this app shape");

  console.log(`CROSS-WORKTREE CACHE VERIFIED: ${afterA.size} shared entries, B served A's bytes, zero recompiles`);
} catch (err) {
  failed = true;
  console.error(`FAIL: ${err.message}`);
} finally {
  process.exit(failed ? 1 : 0);
}
