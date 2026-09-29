// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// TanStack Start request handling, dev and the generated prod server: a
// non-GET on a dotted path reaches the app's server route (Vite hands every
// request nothing else owns to the app), bodies stream through as bytes (no
// size cap, binary intact) and every set-cookie header of the response
// survives the proxy.

import { spawn, execSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const app = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const DEV_PORT = Number(process.env.OJ_E2E_PORT || 3094);
const PROD_PORT = DEV_PORT + 1;

const installed =
  fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start")) &&
  fs.existsSync(path.join(app, "node_modules", "rolldown"));
if (!installed) {
  console.log("SKIP start server routes: fixture deps not installed");
  process.exit(0);
}

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const rm = (p) => {
  for (let i = 0; ; i++) {
    try {
      return fs.rmSync(p, { recursive: true, force: true });
    } catch (e) {
      if (i >= 20) throw e;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100);
    }
  }
};
const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

async function assertServerRoutes(port, label) {
  // 6 MB of random bytes: not valid UTF-8, well past any small-body path.
  const blob = randomBytes(6 * 1024 * 1024);
  const res = await fetch(`http://localhost:${port}/api/export.csv`, {
    method: "POST",
    headers: { "content-type": "application/octet-stream" },
    body: blob,
  });
  must(
    res.status === 200,
    `${label}: POST /api/export.csv returned ${res.status} (dotted non-GET must reach the server route)`,
  );
  must(
    res.headers.get("x-body-bytes") === String(blob.length),
    `${label}: the server route saw ${res.headers.get("x-body-bytes")} body bytes, want ${blob.length}`,
  );
  const echoed = Buffer.from(await res.arrayBuffer());
  must(echoed.equals(blob), `${label}: echoed body differs from what was sent (binary body mangled)`);
  const cookies = res.headers.getSetCookie();
  must(
    cookies.length === 2 && cookies[0].startsWith("first=1") && cookies[1].startsWith("second=2"),
    `${label}: expected two set-cookie headers, got ${JSON.stringify(cookies)}`,
  );

  // An unowned dotted PUT is the app's too: its router answers (an HTML 404
  // here), not the dev server's method/route rejection.
  const put = await fetch(`http://localhost:${port}/files/a.b`, { method: "PUT", body: "x" });
  must(
    (put.headers.get("content-type") || "").includes("text/html"),
    `${label}: PUT /files/a.b should reach the app (got ${put.status} ${put.headers.get("content-type")})`,
  );
  console.log(`${label}: dotted POST server route + binary echo + 2 set-cookie + dotted PUT ok`);

  // Behind a proxy that already set x-forwarded-host (a preview or a tunnel),
  // the app must still get a parseable URL: Host is the server's own and the
  // proxy's x-forwarded-host passes through untouched, Vite's shape. Joining
  // the two hosts used to surface as a 500 "Failed to parse URL".
  const viaProxy = await fetch(`http://localhost:${port}/api/request-url`, {
    headers: { "x-forwarded-host": "preview.example.com" },
  });
  must(viaProxy.status === 200, `${label}: GET /api/request-url behind a proxy returned ${viaProxy.status}`);
  const seen = await viaProxy.json();
  must(
    seen.url.startsWith(`http://localhost:${port}/api/request-url`),
    `${label}: request.url should use the server's own host, got ${seen.url}`,
  );
  must(seen.host === `localhost:${port}`, `${label}: Host should be the server's own, got ${seen.host}`);
  must(
    seen.forwardedHost === "preview.example.com",
    `${label}: x-forwarded-host should pass through, got ${seen.forwardedHost}`,
  );
  console.log(`${label}: proxied request keeps Host and passes x-forwarded-host through`);

  // The fixture configures its own Start server entry (src/ssr-entry.ts, a
  // wrapper around the default handler, as an SSR error wrapper is). Vite runs
  // the configured entry; so must oj, in dev and in the prod server bundle.
  for (const route of ["/", "/api/request-url"]) {
    const res = await fetch(`http://localhost:${port}${route}`);
    must(
      res.headers.get("x-server-entry") === "fixture",
      `${label}: ${route} did not go through the configured server entry (x-server-entry=${res.headers.get("x-server-entry")})`,
    );
  }
  console.log(`${label}: configured tanstackStart server.entry wraps the handler`);
}

async function devPhase() {
  rm(path.join(app, ".oj-cache"));
  const srv = spawn(oj, ["dev", app, "--port", String(DEV_PORT)], { stdio: "ignore" });
  try {
    await waitUp(`http://localhost:${DEV_PORT}/`);
    await assertServerRoutes(DEV_PORT, "start-dev");
  } finally {
    srv.kill("SIGKILL");
  }
}

async function prodPhase() {
  rm(path.join(app, "dist"));
  rm(path.join(app, ".oj-cache"));
  execSync(`${oj} build ${app}`, { cwd: repo, stdio: ["ignore", "ignore", "inherit"] });
  const server = fs.readFileSync(path.join(app, "dist", "server.mjs"), "utf8");
  must(
    server.includes("getSetCookie") && server.includes("Readable.fromWeb"),
    "prod: dist/server.mjs should stream responses and keep every set-cookie",
  );
  // environments.{ssr,client}.define reach the matching prod bundle (Vite's
  // define plugin reads environment.config.define per build).
  const chunks = path.join(app, "dist", "chunks");
  const serverBundle = [
    path.join(app, "dist", "server-bundle.mjs"),
    ...(fs.existsSync(chunks) ? fs.readdirSync(chunks).map((f) => path.join(chunks, f)) : []),
  ]
    .filter((f) => f.endsWith(".mjs"))
    .map((f) => fs.readFileSync(f, "utf8"))
    .join("\n");
  // (the minifier may emit either quote style for the string literal)
  const lit = (s) => new RegExp(`["'\`]${s}["'\`]`);
  must(
    lit("server-side").test(serverBundle) && !lit("client-side").test(serverBundle),
    "prod: server bundle did not apply environments.ssr.define",
  );
  const assets = path.join(app, "dist", "client", "assets");
  const clientJs = fs
    .readdirSync(assets)
    .filter((f) => f.endsWith(".js"))
    .map((f) => fs.readFileSync(path.join(assets, f), "utf8"))
    .join("\n");
  must(
    lit("client-side").test(clientJs) && !lit("server-side").test(clientJs),
    "prod: client bundle did not apply environments.client.define",
  );
  const srv = spawn("node", [path.join(app, "dist", "server.mjs")], {
    cwd: app,
    stdio: "ignore",
    env: { ...process.env, PORT: String(PROD_PORT) },
  });
  try {
    await waitUp(`http://localhost:${PROD_PORT}/`);
    await assertServerRoutes(PROD_PORT, "start-prod");
  } finally {
    srv.kill("SIGKILL");
  }
}

try {
  await devPhase();
  await prodPhase();
  console.log("\nSTART SERVER ROUTES PASSED");
} catch (e) {
  console.error("\nSTART SERVER ROUTES FAILED:", e.message);
  process.exit(1);
}
