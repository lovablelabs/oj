// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite parity for the process-level plugin contract:
// 1. `httpServer` emits "listening" only once oj's socket really accepts, and
//    `address()` is null before / the real bound port after — a plugin's
//    once("listening") self-dial (warmup fetchers, tunnel handshakes) must
//    connect instead of being refused.
// 2. `process.exit(code)` in a plugin exits the dev server with that code:
//    plugins share the server process under Vite, and supervisor-style
//    plugins rely on exiting to force a supervised respawn.

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

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-srvproc-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "srvproc", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "index.html"), "<!doctype html><html><body>ok</body></html>");
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `const probe = { addressAtConfigure: "unset", urlsAtConfigure: "unset", listening: false, port: null, interface: null, urls: null, fetchStatus: null, fetchError: null };
export default [{
  name: "test:server-process",
  configureServer(server) {
    const httpServer = server.httpServer;
    probe.addressAtConfigure = httpServer.address();
    probe.urlsAtConfigure = server.resolvedUrls;
    httpServer.once("listening", async () => {
      probe.listening = true;
      const address = httpServer.address();
      probe.port = address && address.port;
      probe.interface = address && address.address;
      probe.urls = server.resolvedUrls;
      try {
        const res = await fetch("http://127.0.0.1:" + probe.port + "/", {
          headers: { accept: "text/html" },
        });
        await res.arrayBuffer();
        probe.fetchStatus = res.status;
      } catch (e) {
        probe.fetchError = String(e);
      }
    });
    server.middlewares.use("/__probe-result", (_req, res) => {
      res.setHeader("content-type", "application/json");
      res.end(JSON.stringify(probe));
    });
    server.middlewares.use("/__exit", (_req, res) => {
      res.end("exiting");
      setTimeout(() => process.exit(7), 150);
    });
  },
}];\n`,
);

const port = 5486;
let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: ["ignore", "inherit", "inherit"] });
const exited = new Promise((resolve) => srv.once("exit", (code, signal) => resolve({ code, signal })));
try {
  for (let i = 0; i < 150; i++) {
    try { if ((await fetch(`http://localhost:${port}/`)).ok) break; } catch {}
    await sleep(200);
  }

  // 1. the listening contract: emitted, real port, self-dial connects.
  let probe = null;
  for (let i = 0; i < 100; i++) {
    const res = await fetch(`http://localhost:${port}/__probe-result`);
    probe = await res.json();
    if (probe.listening && (probe.fetchStatus !== null || probe.fetchError !== null)) break;
    await sleep(200);
  }
  assert.equal(probe.addressAtConfigure, null, "address() must be null before the socket is bound (Vite parity)");
  assert.equal(probe.urlsAtConfigure, null, "resolvedUrls is null until listen (Vite parity)");
  assert.equal(probe.listening, true, `"listening" never fired: ${JSON.stringify(probe)}`);
  assert.equal(probe.port, port, `address().port must be the real bound port: ${JSON.stringify(probe)}`);
  assert.ok(
    probe.interface === "127.0.0.1" || probe.interface === "::1",
    `address().address is the real bind interface: ${JSON.stringify(probe)}`,
  );
  assert.deepEqual(
    probe.urls && probe.urls.local,
    [`http://localhost:${port}/`],
    `resolvedUrls is set before listening callbacks run: ${JSON.stringify(probe)}`,
  );
  assert.equal(probe.fetchError, null, `the listening-time self-dial failed: ${JSON.stringify(probe)}`);
  assert.equal(probe.fetchStatus, 200, `the listening-time self-dial got ${probe.fetchStatus}`);
  console.log("listening parity ok:", JSON.stringify(probe));

  // 2. process.exit(code) takes the dev server down with that code.
  assert.equal(await (await fetch(`http://localhost:${port}/__exit`)).text(), "exiting");
  const done = await Promise.race([exited, sleep(15000).then(() => null)]);
  assert.ok(done, "the dev server did not exit after a plugin called process.exit(7)");
  assert.equal(done.code, 7, `expected exit code 7, got ${JSON.stringify(done)}`);
  console.log("process.exit parity ok:", JSON.stringify(done));
} catch (e) {
  failed = true;
  console.error(e);
} finally {
  srv.kill("SIGKILL");
  await Promise.race([exited, sleep(2000)]);
  for (let i = 0; i < 20; i++) {
    try {
      fs.rmSync(app, { recursive: true, force: true });
      break;
    } catch {
      await sleep(150);
    }
  }
}
process.exit(failed ? 1 : 0);
