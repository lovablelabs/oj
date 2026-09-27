// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Client code that names a node builtin's export — `import { createHmac }
// from "node:crypto"` in a module shared with the server (webhook/session
// auth is the common shape) — must bundle and serve like under Vite: the
// browser-external stub is CommonJS, so the named import interops to an
// undefined property read instead of a rolldown MISSING_EXPORT that fails
// the build (fingerprint family 9322bab6, 38 projects in the 2026-09-27 5k
// campaign). Runs dev against the start-app fixture and `oj build` on it.

import { execSync, spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const fixture = path.join(repo, "e2e", "fixtures", "start-app");

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-client-node-shim-"));
const cleanup = () => fs.rmSync(app, { recursive: true, force: true });
fs.cpSync(fixture, app, {
  recursive: true,
  filter: (src) => !/\/(node_modules|\.oj-cache|dist)(\/|$)/.test(src),
});
try {
  execSync("npm install --no-audit --no-fund --no-package-lock --loglevel=error", { cwd: app, stdio: "ignore" });
} catch {
  console.log("SKIP client-node-shim: could not install the fixture (offline?)");
  cleanup();
  process.exit(0);
}

// A module shared by server and client that NAMES crypto exports; the client
// graph must link it without calling them.
fs.writeFileSync(
  path.join(app, "src", "sign.ts"),
  `import { createHmac, timingSafeEqual } from "node:crypto";

export function sign(secret: string, body: string): string {
  return createHmac("sha256", secret).update(body).digest("hex");
}
export const canSign = typeof createHmac === "function" && typeof timingSafeEqual === "function";
`,
);
fs.writeFileSync(
  path.join(app, "src", "routes", "shim-probe.tsx"),
  `import { createRoute } from "@tanstack/react-router";
import { canSign } from "../sign";

import { rootRoute } from "./__root";

export const shimProbeRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/shim-probe",
  component: () => <main data-testid="shim-probe">{"canSign:" + canSign}</main>,
});
`,
);
const routeTreePath = path.join(app, "src", "routeTree.ts");
const routeTree = fs
  .readFileSync(routeTreePath, "utf8")
  .replace('import { rootRoute }', 'import { shimProbeRoute } from "./routes/shim-probe";\nimport { rootRoute }')
  .replace("requestUrlRoute]", "requestUrlRoute, shimProbeRoute]");
if (!routeTree.includes("shimProbeRoute]")) throw new Error("could not register the test route");
fs.writeFileSync(routeTreePath, routeTree);

const port = 5221;
const server = spawn(oj, ["dev", ".", "--port", String(port), "--host=127.0.0.1"], {
  cwd: app,
  stdio: ["ignore", "pipe", "pipe"],
});
let log = "";
server.stdout.on("data", (d) => (log += d));
server.stderr.on("data", (d) => (log += d));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
try {
  let body;
  let status = 0;
  for (let i = 0; i < 120; i++) {
    try {
      const res = await fetch(`http://127.0.0.1:${port}/shim-probe`);
      status = res.status;
      body = await res.text();
      if (status === 200 || status === 500) break;
    } catch {}
    await sleep(500);
  }
  // SSR runs on the server where node:crypto is real, so canSign is true;
  // the point is that the CLIENT graph (same module) linked and serves.
  if (status !== 200 || !body.includes("canSign:true")) {
    console.error(log.slice(-4000));
    throw new Error(
      `serving a client graph that names node:crypto exports failed: status ${status}` +
        (log.includes("MISSING_EXPORT") ? " (shim link error is back)" : ""),
    );
  }
} finally {
  server.kill("SIGKILL");
}

const built = spawnSync(oj, ["build", "."], { cwd: app, encoding: "utf8" });
const blog = (built.stdout ?? "") + (built.stderr ?? "");
if (built.status !== 0) {
  console.error(blog.slice(-4000));
  cleanup();
  throw new Error(
    `building a client graph that names node:crypto exports failed` +
      (blog.includes("MISSING_EXPORT") ? " (shim link error is back)" : ""),
  );
}
cleanup();
console.log("client-node-shim: ok");
