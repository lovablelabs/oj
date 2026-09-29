// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Client code that names a node builtin's export, `import { createHmac }
// from "node:crypto"` in a module shared with the server (webhook/session
// auth is the common shape), must bundle and serve like under Vite: the
// browser-external stub is CommonJS, so the named import interops to an
// undefined property read instead of a rolldown MISSING_EXPORT that fails
// the build. Runs dev against the start-app fixture and `oj build` on it.

import { execSync, spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const fixture = path.join(repo, "e2e", "fixtures", "start-app");

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-client-node-shim-"));
// The dev server's engine children can outlive a plain parent kill and write
// to .oj-cache during teardown; group-kill plus retried rm keeps this stable.
const cleanup = () => fs.rmSync(app, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
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

// An installed npm package sharing a builtin's name must beat the shim, as
// under Vite (tryNodeResolve runs before the builtin stub) and Node: the
// probe in nodeBuiltinShims resolves first and only stubs on a miss. npm
// `events` is the canonical case.
const eventsPkg = path.join(app, "node_modules", "events");
fs.mkdirSync(eventsPkg, { recursive: true });
fs.writeFileSync(
  path.join(eventsPkg, "package.json"),
  JSON.stringify({ name: "events", version: "3.3.0", main: "index.js" }),
);
fs.writeFileSync(
  path.join(eventsPkg, "index.js"),
  `exports.EventEmitter = class EventEmitter {};\nexports.OJ_NPM_EVENTS = "npm-events";\n`,
);

// A module shared by server and client that NAMES crypto exports; the client
// graph must link it without calling them.
fs.writeFileSync(
  path.join(app, "src", "sign.ts"),
  `import { createHmac, timingSafeEqual } from "node:crypto";
// @ts-ignore the local stub package has no types
import { OJ_NPM_EVENTS } from "events";

export function sign(secret: string, body: string): string {
  return createHmac("sha256", secret).update(body).digest("hex");
}
export const canSign = typeof createHmac === "function" && typeof timingSafeEqual === "function";
export const npmEvents = OJ_NPM_EVENTS;
`,
);
fs.writeFileSync(
  path.join(app, "src", "routes", "shim-probe.tsx"),
  `import { createRoute } from "@tanstack/react-router";
import { canSign, npmEvents } from "../sign";

import { rootRoute } from "./__root";

export const shimProbeRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/shim-probe",
  component: () => <main data-testid="shim-probe">{"canSign:" + canSign + " events:" + npmEvents}</main>,
});
`,
);
const routeTreePath = path.join(app, "src", "routeTree.ts");
const routeTree = fs
  .readFileSync(routeTreePath, "utf8")
  .replace("import { rootRoute }", 'import { shimProbeRoute } from "./routes/shim-probe";\nimport { rootRoute }')
  .replace("requestUrlRoute]", "requestUrlRoute, shimProbeRoute]");
if (!routeTree.includes("shimProbeRoute]")) throw new Error("could not register the test route");
fs.writeFileSync(routeTreePath, routeTree);

const port = 5221;
const server = spawn(oj, ["dev", ".", "--port", String(port), "--host=127.0.0.1"], {
  cwd: app,
  stdio: ["ignore", "pipe", "pipe"],
  detached: true,
});
const killServer = () => {
  try {
    process.kill(-server.pid, "SIGKILL");
  } catch {
    try {
      server.kill("SIGKILL");
    } catch {}
  }
};
let log = "";
server.stdout.on("data", (d) => (log += d));
server.stderr.on("data", (d) => (log += d));
try {
  let body;
  let status = 0;
  await waitUp(`http://127.0.0.1:${port}/shim-probe`, {
    until: async (res) => {
      status = res.status;
      body = await res.text();
      return status === 200 || status === 500;
    },
  }).catch(() => {});
  // SSR runs on the server where node:crypto is real, so canSign is true;
  // the point is that the CLIENT graph (same module) linked and serves.
  if (status !== 200 || !body.includes("canSign:true") || !body.includes("events:npm-events")) {
    console.error(log.slice(-4000));
    throw new Error(
      `serving a client graph that names node:crypto exports failed: status ${status}` +
        (log.includes("MISSING_EXPORT") ? " (shim link error is back)" : "") +
        (body?.includes("events:undefined")
          ? " (the shim swallowed an installed npm package named like a builtin)"
          : ""),
    );
  }

  const built = spawnSync(oj, ["build", "."], { cwd: app, encoding: "utf8" });
  const blog = (built.stdout ?? "") + (built.stderr ?? "");
  if (built.status !== 0) {
    console.error(blog.slice(-4000));
    throw new Error(
      `building a client graph that names node:crypto exports failed` +
        (blog.includes("MISSING_EXPORT") ? " (shim link error is back)" : ""),
    );
  }
} finally {
  killServer();
  cleanup();
}
console.log("client-node-shim: ok");
