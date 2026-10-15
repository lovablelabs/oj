// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// The start entry's presence is a config-time input: upstream resolves
// `#tanstack-start-entry` once in its plugin's `config` hook, and Vite
// restarts the server for any config-shaped file event, create and delete
// included (handleHMRUpdate's config/configDependencies/env branch). oj probes
// the entry once per StartHost and per bundle run, so a `src/start.*` that
// appears or disappears mid-session restarts the dev server the way a config
// edit does; an edit to an existing entry stays an ordinary rebundle. The
// bundle cache re-keys on the probed entry, so the restart boots the real
// entry instead of restoring the stale stub bundle. Run with a built
// target/debug/oj (or OJ_BIN).
import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { restartedBoot, settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = 6867;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const installed =
  fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(fixture, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP start entry restart: fixture deps not installed");
  console.log("  enable with: (cd e2e/fixtures/start-app && npm install)");
  process.exit(0);
}

if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-entry-"));
const app = path.join(tmp, "app");
fs.mkdirSync(app);
for (const f of ["src", "public", "styles", "packages", "tsconfig.json", "package.json", "vite.config.ts"]) {
  fs.cpSync(path.join(fixture, f), path.join(app, f), { recursive: true });
}
fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");

const startFile = path.join(app, "src", "start.ts");
const startSource = (marker) => `import { createMiddleware, createStart } from "@tanstack/react-start";

const mark = createMiddleware({ type: "function" })
  .client(({ next }) => {
    (globalThis as any).${marker} = true;
    return next();
  })
  .server(({ next }) => {
    (globalThis as any).${marker} = true;
    return next();
  });

export const startInstance = createStart(() => ({
  functionMiddleware: [mark],
}));
`;

let log = "";
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
srv.stdout.on("data", (c) => (log += c));
srv.stderr.on("data", (c) => (log += c));

const get = async (route) => {
  const res = await fetch(`http://localhost:${PORT}${route}`);
  return { status: res.status, body: await res.text() };
};

const restarts = () => log.split("restarting dev server").length - 1;
// Only the served chunks, never the bundle cache's blob store: old
// generations legitimately retain bytes the server no longer serves.
const chunksHold = (needle) => {
  const stack = [path.join(app, ".oj-cache")];
  while (stack.length) {
    const dir = stack.pop();
    let entries;
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      continue;
    }
    for (const e of entries) {
      const p = path.join(dir, e.name);
      if (e.isDirectory()) {
        if (e.name === "client-chunks") {
          for (const f of fs.readdirSync(p, { recursive: true })) {
            try {
              if (String(f).endsWith(".js") && fs.readFileSync(path.join(p, String(f)), "utf8").includes(needle)) {
                return true;
              }
            } catch {}
          }
        } else if (e.name !== "blobs" && e.name !== "node_modules") {
          stack.push(p);
        }
      }
    }
  }
  return false;
};

let failed = false;
try {
  await waitUp(`http://localhost:${PORT}/`, { proc: srv });
  if ((await get("/")).status !== 200) throw new Error("boot without src/start.ts did not serve");
  if (chunksHold("OJ_ENTRY_MARKER_ONE")) throw new Error("marker present before the entry exists");

  // A created src/start.ts restarts the dev server (a config-shaped input
  // flipped), and the rebooted client bundle carries the app's entry: the
  // cache re-keys on the probed entry instead of restoring the stub bundle.
  const beforeCreate = log.length;
  fs.writeFileSync(startFile, startSource("OJ_ENTRY_MARKER_ONE"));
  if (!(await settles(() => restarts() >= 1, { timeoutMs: 30000 }))) {
    throw new Error(`creating src/start.ts did not restart; log tail:\n${log.slice(-3000)}`);
  }
  // The entry edit below needs the restarted process watching.
  await restartedBoot(() => log, beforeCreate);
  await waitUp(`http://localhost:${PORT}/`, { proc: srv });
  if (!(await settles(() => chunksHold("OJ_ENTRY_MARKER_ONE"), { timeoutMs: 30000 }))) {
    throw new Error(`the restarted client bundle lacks the created entry; log tail:\n${log.slice(-3000)}`);
  }
  if ((await get("/")).status !== 200) throw new Error("did not serve after the create restart");
  console.log("entry restart: created src/start.ts restarted and bundled the entry");

  // An edit to the existing entry is an ordinary module change: it rebundles
  // (src/start.ts is in the closure now) and must not restart again.
  const restartsAfterCreate = restarts();
  fs.writeFileSync(startFile, startSource("OJ_ENTRY_MARKER_TWO"));
  if (!(await settles(() => chunksHold("OJ_ENTRY_MARKER_TWO"), { timeoutMs: 30000 }))) {
    throw new Error(`editing src/start.ts did not rebundle; log tail:\n${log.slice(-3000)}`);
  }
  await sleep(1000);
  if (restarts() !== restartsAfterCreate) {
    throw new Error(`editing src/start.ts restarted the server; log tail:\n${log.slice(-3000)}`);
  }
  console.log("entry restart: editing src/start.ts rebundled without a restart");

  // Deleting the entry flips back: restart, and the rebooted bundle serves
  // the framework-default stub again.
  fs.rmSync(startFile);
  if (!(await settles(() => restarts() > restartsAfterCreate, { timeoutMs: 30000 }))) {
    throw new Error(`deleting src/start.ts did not restart; log tail:\n${log.slice(-3000)}`);
  }
  await waitUp(`http://localhost:${PORT}/`, { proc: srv });
  if (!(await settles(() => !chunksHold("OJ_ENTRY_MARKER_TWO"), { timeoutMs: 30000 }))) {
    throw new Error(`the deleted entry still serves; log tail:\n${log.slice(-3000)}`);
  }
  if ((await get("/")).status !== 200) throw new Error("did not serve after the delete restart");
  console.log("entry restart: deleted src/start.ts restarted back to the stub");
  console.log("START-ENTRY-RESTART E2E PASSED");
} catch (err) {
  failed = true;
  console.error("START-ENTRY-RESTART E2E FAILED:", err.message);
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
