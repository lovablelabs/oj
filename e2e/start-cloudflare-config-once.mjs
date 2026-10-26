// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// A runner-backed config (@cloudflare/vite-plugin's Worker environment) gets
// Vite's real DevEnvironments, built from the one resolve the plugin host's
// config came from: the vite.config is evaluated once, its plugins' config
// hook runs once, and one workerd serves the Worker environment. In the dev
// server's process only a cold config extraction adds an evaluation, whose
// instances nothing serves (the deps pre-seed is a one-shot child's). Run
// with a built target/debug/oj (or OJ_BIN); OJ_E2E_CF_DEPS names a prepared
// install of CLOUDFLARE_DEPS.
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
const PORT = Number(process.env.OJ_E2E_PORT || 6874);

if (!fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start"))) {
  console.log("SKIP start cloudflare config once: fixture deps not installed");
  process.exit(0);
}
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-cf-once-"));
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

const evals = path.join(tmp, "evals.log");
const original = fs.readFileSync(path.join(fixture, "vite.config.ts"), "utf8");
const config = original
  .replace(
    'import { defineConfig } from "vite";',
    'import { defineConfig } from "vite";\nimport { cloudflare } from "@cloudflare/vite-plugin";',
  )
  .replace(/^(\s*)tanstackStart\(/m, '$1cloudflare({ viteEnvironment: { name: "ssr" } }),\n$1tanstackStart(')
  .replace(
    "plugins: [",
    `plugins: [
    {
      name: "e2e-config-counter",
      config() {
        countFs.appendFileSync(${JSON.stringify(evals)}, \`config \${countId}\\n\`);
      },
      configureServer() {
        countServed();
      },
      buildStart() {
        countServed();
      },
      resolveId() {
        countServed();
      },
    },`,
  );
if (!config.includes("cloudflare(") || !config.includes("e2e-config-counter")) {
  throw new Error("fixture vite.config.ts changed shape; update this script");
}
fs.writeFileSync(
  path.join(app, "vite.config.ts"),
  `import * as countFs from "node:fs";
const countId = process.pid + ":" + Math.random().toString(36).slice(2);
countFs.appendFileSync(${JSON.stringify(evals)}, \`eval \${countId}\\n\`);
let countServedOnce = false;
function countServed() {
  if (countServedOnce) return;
  countServedOnce = true;
  countFs.appendFileSync(${JSON.stringify(evals)}, \`served \${countId}\\n\`);
}
` + config,
);
fs.writeFileSync(evals, "");

const lines = () =>
  fs
    .readFileSync(evals, "utf8")
    .split("\n")
    .filter(Boolean)
    .map((l) => l.split(" "));
const tally = () => {
  const ls = lines().filter(([, id]) => id.startsWith(`${srv.pid}:`));
  const ids = ls.filter(([k]) => k === "eval").map(([, id]) => id);
  const served = new Set(ls.filter(([k]) => k === "served").map(([, id]) => id));
  const configs = (id) => ls.filter(([k, i]) => k === "config" && i === id).length;
  return { ids, served: ids.filter((id) => served.has(id)), reconfigured: ids.filter((id) => configs(id) !== 1) };
};
// The workerd processes descending from the dev server.
const workerds = (root) => {
  const rows = execSync("ps -axo pid=,ppid=,comm=", { encoding: "utf8" })
    .split("\n")
    .map((l) => l.trim().split(/\s+/))
    .filter((r) => r.length >= 3)
    .map(([pid, ppid, comm]) => ({ pid: Number(pid), ppid: Number(ppid), comm }));
  const mine = new Set([root]);
  for (let grew = true; grew;) {
    grew = false;
    for (const r of rows) if (!mine.has(r.pid) && mine.has(r.ppid)) (mine.add(r.pid), (grew = true));
  }
  return rows.filter((r) => mine.has(r.pid) && /workerd$/.test(r.comm)).length;
};

let log = "";
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], {
  cwd: app,
  detached: true,
  stdio: ["ignore", "pipe", "pipe"],
  env: { ...process.env, WRANGLER_SEND_METRICS: "false", CI: "1", NO_COLOR: "1" },
});
srv.stdout.on("data", (c) => (log += c));
srv.stderr.on("data", (c) => (log += c));

let failed = false;
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
  if (!/a custom dev environment is declared/.test(log)) throw new Error(`no runner environment:\n${log.slice(-4000)}`);
  // The Worker environment served the document: its plugins ran.
  await settles(() => tally().served.length >= 1, { timeoutMs: 20000 });
  await sleep(1000);
  const t = tally();
  if (t.served.length !== 1) {
    throw new Error(`${t.ids.length} evaluations, ${t.served.length} serving plugins; Vite's one expected`);
  }
  if (t.ids.length > 2)
    throw new Error(`${t.ids.length} evaluations, at most the host's and a cold extraction expected`);
  if (t.reconfigured.length) throw new Error(`config hook not run once for ${t.reconfigured.join(", ")}`);
  const n = workerds(srv.pid);
  if (n !== 1) throw new Error(`${n} workerd processes, one expected`);
  console.log(`start-cloudflare-config-once: ${t.ids.length} evaluation(s), 1 serving, one workerd`);
  console.log("START-CLOUDFLARE-CONFIG-ONCE E2E PASSED");
} catch (err) {
  failed = true;
  console.error("START-CLOUDFLARE-CONFIG-ONCE E2E FAILED:", err.message);
} finally {
  try {
    process.kill(-srv.pid, "SIGTERM");
  } catch {}
  await sleep(3000);
  try {
    process.kill(-srv.pid, "SIGKILL");
  } catch {}
  fs.rmSync(tmp, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
}
process.exit(failed ? 1 : 0);
