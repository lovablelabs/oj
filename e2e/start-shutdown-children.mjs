// SPDX-License-Identifier: MIT
// Stopping a TanStack Start dev server must not orphan what its plugins spawned.
// @cloudflare/vite-plugin starts workerd from configureServer and disposes it
// from the server.close() it wraps; oj's Start path used to exit on the signal
// without calling either, leaving one workerd per stop. The fixture plugin
// spawns two children: one its (async) wrapped server.close() kills, the
// miniflare shape, and one nothing in the plugin ever kills. It also listens
// for SIGTERM and re-raises it on exit the way signal-exit does, which used to
// let Deno's default action kill oj mid-close. Unix only (sleep, SIGTERM, 143).
//
//   close settles  → oj exits 143, the wrapped close ran once while its child
//                    was alive, buildEnd and closeBundle ran, both children gone
//   close hangs    → oj still exits 143 within the 5 s bound plus the sweep,
//                    both children gone
//   self-SIGTERM   → a plugin signalling its own pid (from a request) still stops oj cleanly

import { execSync, spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-gating-app");
const OJ = path.join(repo, "target", "debug", "oj");
const PORT = 5352;

if (process.platform === "win32") {
  console.log("SKIP start-shutdown-children (unix only)");
  process.exit(0);
}
const sharedDeps = path.join(here, "fixtures", "start-app", "node_modules");
if (!fs.existsSync(sharedDeps)) {
  console.log("SKIP start-shutdown-children (start-app fixture not installed)");
  process.exit(0);
}
execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const alive = (pid) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
};
async function gone(pid, ms = 5000) {
  const t0 = Date.now();
  while (alive(pid)) {
    if (Date.now() - t0 > ms) return false;
    await sleep(50);
  }
  return true;
}

function writeApp(app, marks) {
  fs.writeFileSync(
    path.join(app, "vite.config.ts"),
    `import { defineConfig } from "vite";
import { spawn } from "node:child_process";
import { appendFileSync, writeFileSync } from "node:fs";
import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";

const log = (line: string) => appendFileSync(${JSON.stringify(marks.events)}, line + "\\n");
const isAlive = (pid?: number) => { try { process.kill(pid!, 0); return true; } catch { return false; } };

export default defineConfig({
  server: { strictPort: true },
  plugins: [
    {
      name: "fake-runtime",
      configureServer(server) {
        const disposable = spawn("sleep", ["600"], { stdio: "ignore" });
        const stubborn = spawn("sleep", ["600"], { stdio: "ignore" });
        writeFileSync(${JSON.stringify(marks.pids)}, JSON.stringify({ disposable: disposable.pid, stubborn: stubborn.pid }));
        // signal-exit's shape: observe the signal, then re-raise it on the own pid once unloaded.
        const onTerm = () => { process.off("SIGTERM", onTerm); setTimeout(() => process.kill(process.pid, "SIGTERM"), 50); };
        process.on("SIGTERM", onTerm);
        server.middlewares.use((req, res, next) => {
          if (req.url !== "/__selfterm") return next();
          res.end("ok");
          process.kill(process.pid, "SIGTERM");
        });
        const close = server.close.bind(server);
        server.close = async () => {
          log("close-start:" + (isAlive(disposable.pid) ? "alive" : "dead"));
          await close();
          if (process.env.FAKE_CLOSE_HANGS) await new Promise(() => {});
          await new Promise((r) => setTimeout(r, 300));
          disposable.kill("SIGTERM");
          log("close-done");
        };
      },
      buildEnd() { log("buildEnd"); },
      closeBundle() { log("closeBundle"); },
    },
    tanstackStart(),
    react(),
  ],
});
`,
  );
}

async function scenario(name, env, check) {
  const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-shutdown-"));
  fs.cpSync(fixture, app, { recursive: true, filter: (src) => !/\/(node_modules|\.oj-cache|dist)(\/|$)/.test(src) });
  fs.symlinkSync(sharedDeps, path.join(app, "node_modules"));
  const marks = { pids: path.join(app, "children.json"), events: path.join(app, "events.log") };
  writeApp(app, marks);
  let child;
  let pids = null;
  try {
    let stderr = "";
    child = spawn(OJ, ["dev", ".", "--port", String(PORT)], {
      cwd: app,
      stdio: ["ignore", "ignore", "pipe"],
      env: { ...process.env, ...env },
    });
    child.stderr.on("data", (d) => (stderr += d.toString()));
    const exited = new Promise((res) => child.on("exit", (code, signal) => res({ code, signal })));
    let outcome = null;
    exited.then((o) => (outcome = o));
    let spawnErr = null;
    child.on("error", (e) => (spawnErr = e));
    const t0 = Date.now();
    for (;;) {
      if (!pids && fs.existsSync(marks.pids)) {
        try {
          pids = JSON.parse(fs.readFileSync(marks.pids, "utf8"));
        } catch {}
      }
      if (pids) {
        try {
          const r = await fetch(`http://localhost:${PORT}/`, { signal: AbortSignal.timeout(2000) });
          if (r.status < 500) break;
        } catch {}
      }
      if (spawnErr) throw spawnErr;
      if (outcome) throw new Error(`oj exited before it was up: ${JSON.stringify(outcome)}\n${stderr.slice(-3000)}`);
      if (Date.now() - t0 > 180_000) throw new Error(`oj did not come up\n${stderr.slice(-3000)}`);
      await sleep(200);
    }
    if (env.FAKE_SELF_SIGTERM) await fetch(`http://localhost:${PORT}/__selfterm`).catch(() => {});
    else child.kill("SIGTERM");
    const result = await Promise.race([exited, sleep(20000).then(() => null)]);
    if (!result) throw new Error("oj did not exit within 20s of SIGTERM");
    if (result.code !== 143)
      throw new Error(`exit with the shell's code 143, not ${JSON.stringify(result)}\n${stderr.slice(-3000)}`);
    for (const [which, pid] of Object.entries(pids)) {
      if (!(await gone(pid))) throw new Error(`the ${which} child (pid ${pid}) outlived oj`);
    }
    const events = fs.existsSync(marks.events) ? fs.readFileSync(marks.events, "utf8").trim().split("\n") : [];
    check(events);
    child = null;
    console.log(`  ok  ${name}`);
  } finally {
    if (child) child.kill("SIGKILL");
    for (const pid of Object.values(pids ?? {})) {
      try {
        process.kill(pid, "SIGKILL");
      } catch {}
    }
    await sleep(200);
    fs.rmSync(app, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
}

const expect = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

let failed = false;
try {
  await scenario("close settles", {}, (ev) => {
    // A buildEnd from startup's client build may precede; the stop is what follows close-start.
    const stop = ev.slice(ev.indexOf("close-start:alive"));
    const want = ["close-start:alive", "close-done", "buildEnd", "closeBundle"];
    expect(JSON.stringify(stop) === JSON.stringify(want), `the stop ran ${want.join(", ")}: got ${ev.join(", ")}`);
  });
  await scenario("close hangs", { FAKE_CLOSE_HANGS: "1" }, (ev) => {
    expect(
      ev.includes("close-start:alive") && !ev.includes("close-done"),
      `the close started and never settled: ${ev.join(", ")}`,
    );
  });
  await scenario("plugin self-SIGTERM", { FAKE_SELF_SIGTERM: "1" }, () => {});
  console.log("START-SHUTDOWN-CHILDREN E2E PASSED");
} catch (err) {
  failed = true;
  console.error("START-SHUTDOWN-CHILDREN E2E FAILED:", err.message);
}
process.exit(failed ? 1 : 0);
