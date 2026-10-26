// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Runner-environment prewarm: when a plugin's worker environments serve the
// documents, warming oj's own engine is wasted — but skipping entirely left
// the first request to pay the whole cold graph (81s on a contended sandbox).
// oj fires two warms with no request in sight: the app's own `server.warmup`
// files through each runner environment's `warmupRequest` (Vite's warmup:
// the environment's own list, never every route), and one real GET / through
// its own listener (the full serving path: middleware forward, worker
// render). The in-host client environment is never warmed: oj's own pipeline
// serves the browser. This runs a copy of the start-gating-app fixture whose
// config declares a fake runner environment that records warmupRequest urls,
// a plugin recording the client environment's transforms, and a
// configureServer middleware that records document hits — and asserts the
// warms fire without any client request and warm only what Vite would.
// The config also mentions @cloudflare/vite-plugin (a comment): the static
// cf hint that arms the prewarm hold, exactly as a real Cloudflare app would.

import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-gating-app");
const OJ = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = 5351;

const sharedDeps = path.join(here, "fixtures", "start-app", "node_modules");
if (!fs.existsSync(sharedDeps)) {
  console.log("SKIP runner-env-prewarm (start-app fixture not installed)");
  process.exit(0);
}

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-runner-warm-"));
const cleanup = () => fs.rmSync(app, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
fs.cpSync(fixture, app, {
  recursive: true,
  filter: (src) => !/\/(node_modules|\.oj-cache|dist)(\/|$)/.test(src),
});
fs.symlinkSync(sharedDeps, path.join(app, "node_modules"));

const warmLog = path.join(app, "warmed.log");
const hitLog = path.join(app, "hits.log");
const clientLog = path.join(app, "client-transforms.log");
fs.writeFileSync(
  path.join(app, "vite.config.ts"),
  `// runner-shaped config; the cf hint below arms the prewarm hold like a
// real app using @cloudflare/vite-plugin.
import { defineConfig } from "vite";
import { appendFileSync } from "node:fs";
import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  server: { warmup: { ssrFiles: ["./src/router.tsx"] } },
  environments: {
    ssr: {
      dev: {
        createEnvironment(name: string) {
          return {
            name,
            async init() {},
            warmupRequest(url: string) {
              appendFileSync(${JSON.stringify(warmLog)}, url + "\\n");
              return Promise.resolve();
            },
          } as any;
        },
      },
    },
  },
  plugins: [
    {
      name: "client-environment-transforms",
      transform(code: string, id: string) {
        const env = (this as any).environment;
        if (env?.name === "client" && typeof env.warmupRequest === "function") {
          appendFileSync(${JSON.stringify(clientLog)}, id + "\\n");
        }
      },
    },
    {
      name: "runner-doc-middleware",
      configureServer(server) {
        server.middlewares.use((req, res, next) => {
          if (req.url === "/" || req.url?.startsWith("/?")) {
            appendFileSync(${JSON.stringify(hitLog)}, req.url + "\\n");
            res.setHeader("content-type", "text/html");
            res.end("<html><body>runner ok</body></html>");
            return;
          }
          next();
        });
      },
    },
    tanstackStart(),
    react(),
  ],
});
`,
);

let failed = false;
let child;
try {
  let stderr = "";
  child = spawn(OJ, ["dev", ".", "--port", String(PORT)], {
    cwd: app,
    stdio: ["ignore", "ignore", "pipe"],
    detached: true,
    env: { ...process.env, OJ_BOOT_PHASES: "1" },
  });
  child.stderr.on("data", (d) => (stderr += d.toString()));
  // A missing/non-executable binary emits an async 'error' event; unhandled,
  // it would crash past the finally (leaked tmpdir, raw stack).
  let spawnErr = null;
  child.on("error", (e) => (spawnErr = e));
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  // No client request is ever made: both records must appear on their own.
  const t0 = Date.now();
  let warmed = "";
  let hits = "";
  for (;;) {
    warmed = fs.existsSync(warmLog) ? fs.readFileSync(warmLog, "utf8") : "";
    hits = fs.existsSync(hitLog) ? fs.readFileSync(hitLog, "utf8") : "";
    if (warmed.includes("/src/router.tsx") && /^\/(\?|$)/m.test(hits)) break;
    if (spawnErr) throw spawnErr;
    if (child.exitCode !== null) throw new Error(`oj exited early\n${stderr.slice(-4000)}`);
    if (Date.now() - t0 > 180_000) {
      throw new Error(
        `prewarm did not fire on its own` +
          ` (warmed=${JSON.stringify(warmed)}, hits=${JSON.stringify(hits)})\n` +
          stderr.slice(-4000),
      );
    }
    await sleep(250);
  }
  if (warmed.includes("/src/routes/")) {
    throw new Error(`route modules warmed beyond server.warmup: ${warmed}`);
  }
  // Long enough for a warm of the client environment to have transformed.
  await sleep(3000);
  const client = fs.existsSync(clientLog) ? fs.readFileSync(clientLog, "utf8") : "";
  if (client) throw new Error(`the in-host client environment transformed at boot:\n${client}`);
  if (!stderr.includes("prewarm: engine skipped (worker environments)")) {
    throw new Error(`engine prewarm was not skipped for the runner env\n${stderr.slice(-2000)}`);
  }
  console.log("runner-env-prewarm: ok (server.warmup + render warm fired unprompted, client environment untouched)");
} catch (e) {
  failed = true;
  console.error(e.message ?? e);
} finally {
  try {
    process.kill(-child.pid, "SIGKILL");
  } catch {
    try {
      child?.kill("SIGKILL");
    } catch {}
  }
  cleanup();
}
process.exit(failed ? 1 : 0);
