// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Vite parity: `configureServer` plugins own raw WebSocket upgrades on the
// shared httpServer (tunnel-style plugins pipe them to an upstream). oj's
// Rust listener must relay unclaimed browser upgrades to the plugin
// middleware server as real `upgrade` events, for paths under a `ws: false`
// proxy entry (the tunnel-over-proxy-prefix shape) and for plain paths alike.

import { spawn, execSync } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import http from "node:http";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

// ---- upstream: a minimal raw WebSocket server (handshake + one greeting
// frame + a reply per client frame), recording the tunneled request paths.
const seenPaths = [];
const upstream = net.createServer((sock) => {
  let buf = Buffer.alloc(0);
  let shaken = false;
  sock.on("data", (chunk) => {
    if (shaken) {
      // Any masked client frame triggers a reply; no need to decode it.
      sock.write(wsTextFrame("got-client-frame"));
      return;
    }
    buf = Buffer.concat([buf, chunk]);
    const end = buf.indexOf("\r\n\r\n");
    if (end === -1) return;
    const head = buf.slice(0, end).toString();
    const pathLine = head.split("\r\n")[0].split(" ")[1];
    seenPaths.push(pathLine);
    const key = /Sec-WebSocket-Key: (.+)/i.exec(head)?.[1]?.trim();
    const accept = crypto
      .createHash("sha1")
      .update(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11")
      .digest("base64");
    shaken = true;
    sock.write(
      "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n" +
        `Sec-WebSocket-Accept: ${accept}\r\n\r\n`,
    );
    sock.write(wsTextFrame("hello-from-upstream"));
  });
});
await new Promise((r) => upstream.listen(0, "127.0.0.1", r));
const upstreamPort = upstream.address().port;

// A plain http upstream for the proxy's non-ws traffic (control case).
const httpUpstream = http.createServer((_req, res) => res.end("proxied-http-ok"));
await new Promise((r) => httpUpstream.listen(0, "127.0.0.1", r));
const httpUpstreamPort = httpUpstream.address().port;

function wsTextFrame(text) {
  const payload = Buffer.from(text);
  return Buffer.concat([Buffer.from([0x81, payload.length]), payload]);
}
function wsMaskedTextFrame(text) {
  const payload = Buffer.from(text);
  const mask = crypto.randomBytes(4);
  const masked = Buffer.from(payload.map((b, i) => b ^ mask[i % 4]));
  return Buffer.concat([Buffer.from([0x81, 0x80 | payload.length]), mask, masked]);
}

// ---- the app: a tunnel-style plugin owning /tunnel/* and /api/ws/* upgrades,
// piping raw sockets to the upstream (the technique real tunnel plugins use).
const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-wsup-"));
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "wsup", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "index.html"), "<!doctype html><html><body>ok</body></html>");
fs.writeFileSync(
  path.join(app, "oj.config.json"),
  JSON.stringify({
    server: { proxy: { "/api": { target: `http://127.0.0.1:${httpUpstreamPort}` } } },
  }),
);
fs.writeFileSync(
  path.join(app, "oj.plugins.mjs"),
  `import net from "node:net";
export default [{
  name: "test:ws-tunnel",
  configureServer(server) {
    const httpServer = server.httpServer;
    if (!httpServer) return;
    httpServer.prependListener("upgrade", (req, socket, head) => {
      const url = req.url ?? "";
      if (!url.startsWith("/tunnel/") && !url.startsWith("/api/ws/")) return;
      const write = socket.write.bind(socket);
      const destroy = socket.destroy.bind(socket);
      socket.write = () => true;
      socket.end = () => socket;
      socket.destroy = () => socket;
      delete req.headers.upgrade;
      delete req.headers.connection;
      const upstream = net.connect(${upstreamPort}, "127.0.0.1", () => {
        const lines = [(req.method ?? "GET") + " " + url + " HTTP/1.1"];
        for (let i = 0; i < req.rawHeaders.length; i += 2) {
          lines.push(req.rawHeaders[i] + ": " + req.rawHeaders[i + 1]);
        }
        upstream.write(lines.join("\\r\\n") + "\\r\\n\\r\\n");
        if (head.length > 0) upstream.write(head);
        socket.on("data", (c) => upstream.write(c));
        upstream.on("data", (c) => write(c));
      });
      upstream.on("error", () => destroy());
      socket.on("error", () => upstream.destroy());
      upstream.on("close", () => destroy());
      socket.on("close", () => upstream.destroy());
    });
  },
}];\n`,
);

// ---- raw ws client through oj
function wsConnect(port, urlPath) {
  return new Promise((resolve, reject) => {
    const key = crypto.randomBytes(16).toString("base64");
    const sock = net.connect(port, "127.0.0.1", () => {
      sock.write(
        `GET ${urlPath} HTTP/1.1\r\nHost: 127.0.0.1:${port}\r\nUpgrade: websocket\r\n` +
          `Connection: Upgrade\r\nSec-WebSocket-Key: ${key}\r\nSec-WebSocket-Version: 13\r\n\r\n`,
      );
    });
    let buf = Buffer.alloc(0);
    let status = null;
    const frames = [];
    sock.on("data", (chunk) => {
      buf = Buffer.concat([buf, chunk]);
      if (status === null) {
        const end = buf.indexOf("\r\n\r\n");
        if (end === -1) return;
        const head = buf.slice(0, end).toString();
        status = parseInt(head.split(" ")[1], 10);
        buf = buf.slice(end + 4);
        if (status !== 101) return resolve({ status, frames, sock });
      }
      // unmasked server frames: 0x81, len, payload
      while (buf.length >= 2 && buf[0] === 0x81) {
        const len = buf[1] & 0x7f;
        if (buf.length < 2 + len) break;
        frames.push(buf.slice(2, 2 + len).toString());
        buf = buf.slice(2 + len);
      }
      resolve({ status, frames, sock });
    });
    sock.on("error", reject);
    setTimeout(() => resolve({ status, frames, sock }), 15000);
  });
}
const waitFrames = async (conn, n, ms = 5000) => {
  const start = Date.now();
  while (conn.frames.length < n && Date.now() - start < ms) await sleep(50);
  return conn.frames;
};

const port = 5488;
let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
try {
  for (let i = 0; i < 150; i++) {
    try { if ((await fetch(`http://localhost:${port}/`)).ok) break; } catch {}
    await sleep(200);
  }

  // 1. plain path: the fallback relay hands the upgrade to the plugin.
  const one = await wsConnect(port, "/tunnel/one");
  assert.equal(one.status, 101, "tunnel path upgrades through the plugin");
  await waitFrames(one, 1);
  assert.equal(one.frames[0], "hello-from-upstream");
  one.sock.write(wsMaskedTextFrame("ping"));
  await waitFrames(one, 2);
  assert.equal(one.frames[1], "got-client-frame", "client frames reach the upstream");
  one.sock.destroy();

  // 2. under a `ws: false` proxy prefix: the proxy middleware must yield the
  //    upgrade to the plugin instead of forwarding it as plain HTTP.
  const two = await wsConnect(port, "/api/ws/two");
  assert.equal(two.status, 101, "proxied-prefix upgrade reaches the plugin, not the HTTP proxy");
  await waitFrames(two, 1);
  assert.equal(two.frames[0], "hello-from-upstream");
  two.sock.destroy();

  // 3. control: plain HTTP through the same proxy entry still proxies.
  const resp = await fetch(`http://127.0.0.1:${port}/api/anything`);
  assert.equal(await resp.text(), "proxied-http-ok", "HTTP proxying unaffected");

  assert.deepEqual(seenPaths, ["/tunnel/one", "/api/ws/two"], "tunneled paths arrive verbatim");

  // 4. an upgrade no plugin claims fails cleanly instead of hanging forever.
  const none = await wsConnect(port, "/unclaimed");
  assert.notEqual(none.status, 101, `unclaimed upgrade must not 101 (got ${none.status})`);
  none.sock.destroy();

  console.log("PLUGIN WS UPGRADE E2E PASSED");
} catch (e) {
  failed = true;
  console.error(e);
} finally {
  srv.kill();
  upstream.close();
  httpUpstream.close();
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
