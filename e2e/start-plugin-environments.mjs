// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// Vite's createServer gives every environment a container over ONE resolved
// plugin array: configureServer runs once, the dev server starts the plugins
// on the client environment only (perEnvironmentStartEndDuringDev opts one
// into the others), and the ssr environment's hooks run on the very
// instances the client's ran on, configureServer's server in hand. Start dev
// runs its client bundle and its SSR on those same instances. Run with a
// built target/debug/oj (or OJ_BIN).
import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { settles, sleep, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 6873);

if (!fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start"))) {
  console.log("SKIP start plugin environments: fixture deps not installed");
  process.exit(0);
}
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "oj-start-plugin-envs-"));
const app = path.join(tmp, "app");
fs.mkdirSync(app);
for (const f of [
  "src",
  "public",
  "styles",
  "packages",
  "tsconfig.json",
  "package.json",
  "vite.config.ts",
  ".env.development",
]) {
  fs.cpSync(path.join(fixture, f), path.join(app, f), { recursive: true });
}
fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");

const hooks = path.join(tmp, "hooks.log");
const configFile = path.join(app, "vite.config.ts");
const source = fs.readFileSync(configFile, "utf8").replace(
  "plugins: [",
  `plugins: [
    {
      name: "e2e-environments",
      configureServer(server) {
        envServer = server;
        envLog("configureServer");
      },
      buildStart() {
        envLog("buildStart " + this.environment.name);
      },
      transform(code, id) {
        if (!id.replace(/\\\\/g, "/").endsWith("/src/routes/index.tsx")) return;
        const env = this.environment.name;
        if (env === "client") envClientSaw = true;
        envLog("transform " + env + " clientSaw=" + envClientSaw + " server=" + !!(envServer && envServer.ws));
      },
    },
    {
      name: "e2e-per-environment",
      perEnvironmentStartEndDuringDev: true,
      buildStart() {
        envLog("per-environment buildStart " + this.environment.name);
      },
    },`,
);
const header = `import * as envFs from "node:fs";
const envLog = (line) => envFs.appendFileSync(${JSON.stringify(hooks)}, line + "\\n");
let envServer = null;
let envClientSaw = false;
`;
fs.writeFileSync(configFile, header + source);
fs.writeFileSync(hooks, "");

let log = "";
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
srv.stdout.on("data", (c) => (log += c));
srv.stderr.on("data", (c) => (log += c));

const lines = () => fs.readFileSync(hooks, "utf8").split("\n").filter(Boolean);
const count = (line) => lines().filter((l) => l === line).length;

let failed = false;
try {
  await waitUp(`http://localhost:${PORT}/`, { proc: srv });
  const page = await fetch(`http://localhost:${PORT}/`);
  if (page.status !== 200) throw new Error(`/ returned ${page.status}`);
  const ssrSeen = () => lines().some((l) => l.startsWith("transform ssr"));
  if (!(await settles(ssrSeen, { timeoutMs: 20000 }))) throw new Error(`no ssr transform:\n${lines().join("\n")}`);
  // Let any late hook land before counting.
  await sleep(1000);
  const all = lines().join("\n");
  const expect = (what, n) => {
    if (count(what) !== n) throw new Error(`expected ${n} x "${what}", got ${count(what)}:\n${all}`);
  };
  expect("configureServer", 1);
  expect("buildStart client", 1);
  expect("buildStart ssr", 0);
  expect("per-environment buildStart client", 1);
  expect("per-environment buildStart ssr", 1);
  if (!lines().includes("transform client clientSaw=true server=true")) {
    throw new Error(`the client bundle's transform did not run on the server's instance:\n${all}`);
  }
  if (!lines().includes("transform ssr clientSaw=true server=true")) {
    throw new Error(`the ssr transform did not run on the client's instance:\n${all}`);
  }
  console.log("START-PLUGIN-ENVIRONMENTS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("START-PLUGIN-ENVIRONMENTS E2E FAILED:", err.message);
  console.error(log.slice(-3000));
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(tmp, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
}
process.exit(failed ? 1 : 0);
