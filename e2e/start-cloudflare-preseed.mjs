// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// The boot-time deps optimization of a runner-backed config runs in one-shot
// children, never in the dev server: rolldown keeps the native memory of
// every build until its process exits. That includes the deps a plugin hands
// the optimizer while the server sets up (@cloudflare/vite-plugin's
// nodejs_compat polyfills, registered from its configureServer): on a cold
// cache they end up optimized, by a child, and the dev server's own
// optimizer never reruns for them. Run with a built target/debug/oj (or
// OJ_BIN); OJ_E2E_CF_DEPS names a prepared install of CLOUDFLARE_DEPS.
import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { CLOUDFLARE_DEPS, settles, sleep } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 6875);

if (!fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start"))) {
  console.log("SKIP start cloudflare preseed: fixture deps not installed");
  process.exit(0);
}
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-cf-preseed-"));
const app = path.join(tmp, "app");
if (process.env.OJ_E2E_CF_DEPS) {
  fs.symlinkSync(path.join(path.resolve(process.env.OJ_E2E_CF_DEPS), "node_modules"), path.join(tmp, "node_modules"));
} else {
  fs.writeFileSync(
    path.join(tmp, "package.json"),
    JSON.stringify({ name: "oj-cf-deps", private: true, type: "module" }),
  );
  execSync(`npm install --no-audit --no-fund --no-package-lock ${CLOUDFLARE_DEPS}`, { cwd: tmp, stdio: "inherit" });
}
fs.mkdirSync(app);
for (const f of ["src", "public", "styles", "packages", "tsconfig.json", "package.json"]) {
  fs.cpSync(path.join(fixture, f), path.join(app, f), { recursive: true });
}
fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");
fs.writeFileSync(
  path.join(app, "wrangler.jsonc"),
  JSON.stringify({
    name: "oj-start-fixture",
    main: "@tanstack/react-start/server-entry",
    compatibility_date: "2025-09-01",
    compatibility_flags: ["nodejs_compat"],
  }),
);
// A deps cache of the app's own: cold, and no other run's.
const cacheDir = path.join(app, ".vite-deps");
const config = fs
  .readFileSync(path.join(fixture, "vite.config.ts"), "utf8")
  .replace(
    'import { defineConfig } from "vite";',
    'import { defineConfig } from "vite";\nimport { cloudflare } from "@cloudflare/vite-plugin";',
  )
  .replace(/^(\s*)tanstackStart\(/m, '$1cloudflare({ viteEnvironment: { name: "ssr" } }),\n$1tanstackStart(')
  .replace("export default defineConfig({", `export default defineConfig({\n  cacheDir: ${JSON.stringify(cacheDir)},`);
if (!config.includes("cloudflare(") || !config.includes("cacheDir:")) {
  throw new Error("fixture vite.config.ts changed shape; update this script");
}
fs.writeFileSync(path.join(app, "vite.config.ts"), config);

// A workspace package whose dependency only the package's own node_modules
// holds (a strict install's shape): the session optimizes it through the
// package's import, but its bare id does not resolve from the app's root.
const inner = path.join(app, "packages", "inner");
const innerDep = path.join(inner, "node_modules", "oj-e2e-inner-dep");
fs.mkdirSync(innerDep, { recursive: true });
fs.writeFileSync(path.join(inner, "package.json"), JSON.stringify({ name: "oj-e2e-inner", type: "module" }));
fs.writeFileSync(path.join(inner, "index.js"), 'export { innerValue } from "oj-e2e-inner-dep";\n');
fs.writeFileSync(
  path.join(innerDep, "package.json"),
  JSON.stringify({ name: "oj-e2e-inner-dep", type: "module", exports: "./index.js" }),
);
fs.writeFileSync(path.join(innerDep, "index.js"), 'export const innerValue = "inner-dep-ok";\n');
const route = path.join(app, "src", "routes", "index.tsx");
fs.writeFileSync(route, `import "../../packages/inner/index.js";\n${fs.readFileSync(route, "utf8")}`);

const metadata = path.join(cacheDir, "deps_ssr", "_metadata.json");
const optimized = () => {
  try {
    return Object.keys(JSON.parse(fs.readFileSync(metadata, "utf8")).optimized ?? {});
  } catch {
    return [];
  }
};
const polyfill = (id) => id.startsWith("@cloudflare/unenv-preset/") || id.startsWith("unenv/");

// One `oj dev` session, up and serving `/`; returns its log once stopped.
async function boot(check) {
  let log = "";
  const srv = spawn(oj, ["dev", app, "--port", String(PORT)], {
    cwd: app,
    detached: true,
    stdio: ["ignore", "pipe", "pipe"],
    // The default cache root: the test reads the stamp and snapshot there.
    env: { ...process.env, OJ_CACHE_DIR: undefined, WRANGLER_SEND_METRICS: "false", CI: "1", NO_COLOR: "1" },
  });
  srv.stdout.on("data", (c) => (log += c));
  srv.stderr.on("data", (c) => (log += c));
  try {
    const up = await settles(
      async () => {
        if (srv.exitCode != null) return true;
        try {
          return (await fetch(`http://127.0.0.1:${PORT}/`)).status > 0;
        } catch {
          return false;
        }
      },
      { timeoutMs: 180000, pollMs: 500 },
    );
    if (!up || srv.exitCode != null) throw new Error(`oj dev did not serve; log:\n${log.slice(-4000)}`);
    if (!/a custom dev environment is declared/.test(log))
      throw new Error(`no runner environment:\n${log.slice(-4000)}`);
    await check(() => log);
    return log;
  } finally {
    try {
      process.kill(-srv.pid, "SIGTERM");
    } catch {}
    await sleep(3000);
    try {
      process.kill(-srv.pid, "SIGKILL");
    } catch {}
  }
}

// What the dev server logged after the boot-time seed: the dev server must
// hash the config the child hashed, or its optimizer throws the child's
// metadata away and optimizes again in process. The first seed line, since
// a mismatch can start a setup-registration child that logs another.
const afterSeed = (log) => {
  const at = log.indexOf("deps pre-optimized for environment");
  if (at < 0) throw new Error(`the child seeded nothing:\n${log.slice(-3000)}`);
  return log.slice(at);
};
const lines = (log, re) => log.split("\n").filter((l) => re.test(l));

let failed = false;
try {
  await boot(async (log) => {
    if (!(await settles(() => optimized().some(polyfill), { timeoutMs: 30000 }))) {
      throw new Error(`the setup's polyfills were never optimized: ${optimized().join(", ")}\n${log().slice(-3000)}`);
    }
    if (!(await settles(() => optimized().includes("oj-e2e-inner-dep"), { timeoutMs: 30000 }))) {
      throw new Error(`the workspace package's dep was never optimized: ${optimized().join(", ")}`);
    }
    // Long enough for a rerun of the dev server's own optimizer to log.
    await sleep(3000);
    // The session finds the workspace package's dep at runtime, as Vite's
    // own session would; nothing else may run the optimizer in process.
    const inProcess = lines(log(), /dependenc(y|ies) optimized/).filter((l) => !/optimized: oj-e2e-inner-dep$/.test(l));
    if (inProcess.length) throw new Error(`the dev server's optimizer ran in process:\n${inProcess.join("\n")}`);
    const rehashed = lines(log(), /Re-optimizing dependencies/);
    if (rehashed.length) throw new Error(`the dev server rejected the child's deps cache:\n${rehashed.join("\n")}`);
  });
  // A second cold boot replays the first session's deps: by name through the
  // include snapshot, only ids Vite's include resolver reaches from the root;
  // the workspace package's dep through the file it resolved to, registered
  // on the child's optimizer, so the dev server never optimizes it again.
  fs.rmSync(path.join(app, ".oj-cache", "v1", "optimize-env-stamp.json"));
  await boot(async (log) => {
    await sleep(3000);
    const unresolved = lines(log(), /Failed to resolve dependency/);
    if (unresolved.length) throw new Error(`the replay named unresolvable deps:\n${unresolved.join("\n")}`);
    const rehashed = lines(afterSeed(log()), /Re-optimizing dependencies/);
    if (rehashed.length) throw new Error(`the dev server rejected the child's deps cache:\n${rehashed.join("\n")}`);
    const inProcess = lines(afterSeed(log()), /dependenc(y|ies) optimized/);
    if (inProcess.length) throw new Error(`the dev server's optimizer ran in process:\n${inProcess.join("\n")}`);
    if (!optimized().includes("oj-e2e-inner-dep")) {
      throw new Error(`the child did not commit the workspace package's dep: ${optimized().join(", ")}`);
    }
    if (!/deps: pre-optimizing/.test(log()))
      throw new Error(`the second boot never ran the child:\n${log().slice(-3000)}`);
    // The replay itself still carries every dep the root resolves.
    const snapshot = JSON.parse(fs.readFileSync(path.join(app, ".oj-cache", "v1", "preseed-include.json"), "utf8"));
    const replayed = snapshot.environments?.ssr ?? [];
    if (!replayed.includes("react") || !replayed.some(polyfill) || replayed.includes("oj-e2e-inner-dep")) {
      throw new Error(`unexpected replay set: ${replayed.join(", ")}`);
    }
  });
  console.log(
    `start-cloudflare-preseed: ${optimized().filter(polyfill).length} setup polyfill(s) optimized out of process`,
  );
  console.log("START-CLOUDFLARE-PRESEED E2E PASSED");
} catch (err) {
  failed = true;
  console.error("START-CLOUDFLARE-PRESEED E2E FAILED:", err.message);
} finally {
  fs.rmSync(tmp, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
}
process.exit(failed ? 1 : 0);
