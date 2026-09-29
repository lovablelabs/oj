// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// `server.proxy` parity: a `ws: true` entry tunnels WebSocket connections to
// the target (subprotocol negotiated by the upstream), and HTTP responses are
// streamed through chunk by chunk (server-sent events, long polls) instead of
// being buffered until the upstream finishes.

import { spawn, execSync } from "node:child_process";
import http from "node:http";
import https from "node:https";
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const UPSTREAM = 5494;
const PORT = 5495;
const TLS_UPSTREAM = 5496;

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

// ---- upstream: a slow chunked route and a minimal WebSocket echo server ----
function wsFrame(op, payload) {
  const len = payload.length;
  let head;
  if (len < 126) head = Buffer.from([0x80 | op, len]);
  else { head = Buffer.alloc(4); head[0] = 0x80 | op; head[1] = 126; head.writeUInt16BE(len, 2); }
  return Buffer.concat([head, payload]);
}
function wsParse(buf) {
  const out = []; let off = 0;
  while (off + 2 <= buf.length) {
    const op = buf[off] & 0x0f; const masked = buf[off + 1] & 0x80; let len = buf[off + 1] & 0x7f; off += 2;
    if (len === 126) { len = buf.readUInt16BE(off); off += 2; } else if (len === 127) { len = Number(buf.readBigUInt64BE(off)); off += 8; }
    let mask; if (masked) { mask = buf.subarray(off, off + 4); off += 4; }
    if (off + len > buf.length) break;
    let payload = buf.subarray(off, off + len); off += len;
    if (masked) payload = Buffer.from(payload.map((c, i) => c ^ mask[i % 4]));
    out.push({ op, payload });
  }
  return out;
}
const upstream = http.createServer((req, res) => {
  if (req.url.startsWith("/api/slow")) {
    res.writeHead(200, { "content-type": "text/plain", "x-upstream": "yes" });
    res.write("first");
    setTimeout(() => { res.write("second"); res.end(); }, 800);
    return;
  }
  if (req.url.startsWith("/api/echo")) {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => { res.writeHead(200, { "content-type": "application/json" }); res.end(JSON.stringify({ got: body, host: req.headers.host })); });
    return;
  }
  res.writeHead(404); res.end();
});
function wsUpgrade(req, socket) {
  const key = req.headers["sec-websocket-key"];
  const accept = crypto.createHash("sha1").update(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest("base64");
  const protos = String(req.headers["sec-websocket-protocol"] || "").split(",").map((s) => s.trim()).filter(Boolean);
  const head = ["HTTP/1.1 101 Switching Protocols", "Upgrade: websocket", "Connection: Upgrade", `Sec-WebSocket-Accept: ${accept}`];
  if (protos.includes("chat")) head.push("Sec-WebSocket-Protocol: chat");
  socket.write(head.join("\r\n") + "\r\n\r\n");
  socket.write(wsFrame(1, Buffer.from("hello from upstream " + req.url)));
  if (req.url.startsWith("/keep")) socket.write(wsFrame(1, Buffer.from(`host=${req.headers.host} origin=${req.headers.origin}`)));
  socket.on("data", (buf) => {
    for (const f of wsParse(buf)) {
      if (f.op === 1) socket.write(wsFrame(1, Buffer.from("echo:" + f.payload.toString())));
      else if (f.op === 8) { socket.write(wsFrame(8, f.payload)); socket.end(); }
      else if (f.op === 9) socket.write(wsFrame(10, f.payload));
    }
  });
  socket.on("error", () => {});
}
upstream.on("upgrade", wsUpgrade);
await new Promise((r) => upstream.listen(UPSTREAM, r));

// The same upstream behind TLS with a self-signed localhost certificate (test
// fixture only): a `wss://`/`https://` target with `secure: false` must work,
// and one without it must be refused, as http-proxy's `secure` does.
const TLS_CERT = `-----BEGIN CERTIFICATE-----
MIIDJTCCAg2gAwIBAgIUMaTIEGFPuA20TEXfOZMfRwUxlmkwDQYJKoZIhvcNAQEL
BQAwFDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDkwMzExMzcxMVoXDTM2MDgz
MTExMzcxMVowFDESMBAGA1UEAwwJbG9jYWxob3N0MIIBIjANBgkqhkiG9w0BAQEF
AAOCAQ8AMIIBCgKCAQEAwkhsie1zZjhntJV2ku4eiNGfwvCx+51dzLRHcL6M/5VZ
3qlioKBeKb63TYZCeraHjR0jck61EqR/YVOeH0G6zfpY8cse3VTGBFvZgpBUPDYY
RNZHZnHbGmwU+Km8O6+gJrsTKrp5LoMOGREvOIvJZknEtBHApe5C1rtpmkXn7sVW
QQMfwsLf8vtEimtWwXzuwtYbt4ZftIs524QulsoxWrbAs1s0qKU9/lufvyfehKHC
dxvrrGy5O1oOJc9hJlmbLf2Sn3O+g/bDPtb6IRGEqXTeRVfncv+UdMrDfg9la+/k
+XJWv4GPJlMv0HqUq3xZDn7gYD0FG29fo4zfRaDWOwIDAQABo28wbTAdBgNVHQ4E
FgQUL+7c6iWhc502GftuAXBX7Bho/0UwHwYDVR0jBBgwFoAUL+7c6iWhc502Gftu
AXBX7Bho/0UwDwYDVR0TAQH/BAUwAwEB/zAaBgNVHREEEzARgglsb2NhbGhvc3SH
BH8AAAEwDQYJKoZIhvcNAQELBQADggEBABb5oWRBWwa7RJKPn38MJeXk9hmGVm7G
xaRP4RN4O586C+PYahqAugOW3muqxedwhN4EUKhMrGlKUysMuCkyGw3oFwjDdvzt
THSTu4eC9wirBAzQ+9GqqT82xI1Jne66aAbhYHUU4bGgHYUEDT3AMGiA41t3bFT2
h2amEtDMlynPyZLt/ghroeHbRYai9OPkpP4SyO1dGm0v25l8he/CnyYUqpkoeyXI
5bwsr+xXN3CweZHlQnF/PzIHQ2lzQxb4eCBd0Q//uArbu7lO2Ok3/4r+0Cuuu+DA
IEPsORbVsq1ZE1hgmMK5KBlbmuRaBMimlp4GthShgdLdQOi/4eMQJi0=
-----END CERTIFICATE-----`;
const TLS_KEY = `-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDCSGyJ7XNmOGe0
lXaS7h6I0Z/C8LH7nV3MtEdwvoz/lVneqWKgoF4pvrdNhkJ6toeNHSNyTrUSpH9h
U54fQbrN+ljxyx7dVMYEW9mCkFQ8NhhE1kdmcdsabBT4qbw7r6AmuxMqunkugw4Z
ES84i8lmScS0EcCl7kLWu2maRefuxVZBAx/Cwt/y+0SKa1bBfO7C1hu3hl+0iznb
hC6WyjFatsCzWzSopT3+W5+/J96EocJ3G+usbLk7Wg4lz2EmWZst/ZKfc76D9sM+
1vohEYSpdN5FV+dy/5R0ysN+D2Vr7+T5cla/gY8mUy/QepSrfFkOfuBgPQUbb1+j
jN9FoNY7AgMBAAECggEAEbYVgvttHj/9IEbR4PIpQXLOvDBCIXyGnQ9ARgRxCSm9
4CET9y23d9nFjyEytUonkFM8NIL9Wd46KI69Zv8Qfw+YBS7tuOKuDJ6s9QygST7r
NndMWgf+H+oDfWnH2a8Yi/9Y73fBbV6QLfPVmLORoCwQbRQDOn0+haHfLiu6SZd2
A8TRIK9D4VSDlVucCB2z/ZBwMgP067Mbk2b65vEWOWuTvlhMZbiT4hXLqnqfv1M5
YLyC9CwvYpxv+jkuLgwO8zohVdlheFcZ8T+q0n5k3b00DpygPdZm7MG9DFLmBJeS
+JRT4k6xkUFQAmS1P3eUTptLcwEbE4+L8jpu+0xGYQKBgQDzpm/qXHP4AfIge/g/
7aSPf4a+A8iDOJMOds1lk9skYHvGGguFa5RGi4C483HdvyLbrA/bTcmdfsIJ0xCv
Wmnw+UebQhkuaPPzXpPmNHJRjd8IKBqTlYoHQ1QmGD3Ii8hkriiNjHQwuaZ4CcCv
0amwC/KWnDGWNRl3CCG7QO8rGwKBgQDMIWfqEklPvRYKwn3pyKbQHXpEmD9lVid2
dss2Bae4x4GFu3BCuSLPA4w4bqyAl287SumgdlOZl594Ag3MKQlPWrFbe1E1TKtu
Zsfy5wVV8CezzN5sgTMab/kDE0BiN04E/XvFReGEu8dWSVdg6XhYO2dqFRc8DXOE
TsnkStOTYQKBgQCrqN5+kqaN2+kX49+yQp7HDwUCiK3TbZ+F+EObxkEF7wglOSJW
3MV5sj19kN7vaQOJGz+MtdBPKwhQXakKsjujsC04AKi3HvCIzWCMNvU36ilxmLeo
tRmrJk96C2g0C++ip2Ug3Qzba2ESf2SHOsM/qhs+60qwVjbbuxnw0L3wcwKBgQCR
31lz4udyzQvgWoZCN3pFhJsoQ6giEYQX2uJy02281Q0Q9RZPCCAA0Wc1uJkbN5xs
QadcXNJ3EuwJhWY4vCaEB6pwVlp8/TIQrfA6+65LcFfe3AsifN15CgVnli1PQnhF
hqMZIUv8X3geiECh55Vxb9oB69pztqUTKn6J3pL9YQKBgE5BqjBl6CV6AetcH059
WTN9VvxvVAlHgxZlZH6+D4fJpj+337Gw3ENqmNlv6eo6gm8iwspOUbQ0UkVz3jYT
s8Sxi3vvYYI2MT8DTQagVWd5sOPsKjqVMOQbfCpXqGFnE0OER6+PghVoZPCuKXoG
aKUOFSLyKxi4jhU/0IXH0pli
-----END PRIVATE KEY-----`;
const tlsUpstream = https.createServer({ cert: TLS_CERT, key: TLS_KEY }, (req, res) => {
  res.writeHead(200, { "content-type": "text/plain", "x-upstream": "tls" });
  res.end("tls:" + req.url);
});
tlsUpstream.on("upgrade", wsUpgrade);
await new Promise((r) => tlsUpstream.listen(TLS_UPSTREAM, r));

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-proxyws-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "proxyws-app", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "oj.config.json"), JSON.stringify({ server: { proxy: {
  "/api": { target: `http://localhost:${UPSTREAM}`, ws: true, changeOrigin: true },
  "/tls": { target: `https://localhost:${TLS_UPSTREAM}`, ws: true, changeOrigin: true, secure: false },
  "/strict": { target: `https://localhost:${TLS_UPSTREAM}`, ws: true, changeOrigin: true },
  "/keep": { target: `http://localhost:${UPSTREAM}`, ws: true, rewriteWsOrigin: true },
} } }));
fs.writeFileSync(path.join(app, "src", "main.js"), `window.__OK = 1;\n`);
fs.writeFileSync(path.join(app, "index.html"), `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`);

let failed = false;
const logPath = path.join(os.tmpdir(), "oj-proxyws.log");
const logFd = fs.openSync(logPath, "w");
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", logFd, logFd] });
try {
  await waitUp(`http://localhost:${PORT}/`);

  // streamed response: the first chunk arrives long before the upstream ends
  const t0 = Date.now();
  const res = await fetch(`http://localhost:${PORT}/api/slow`);
  assert.equal(res.headers.get("x-upstream"), "yes");
  const reader = res.body.getReader();
  const first = await reader.read();
  const tFirst = Date.now() - t0;
  let text = new TextDecoder().decode(first.value);
  for (;;) { const { value, done } = await reader.read(); if (done) break; text += new TextDecoder().decode(value); }
  assert.equal(text, "firstsecond");
  assert.ok(tFirst < 500, `first chunk took ${tFirst}ms: response was buffered`);

  // plain request/response still works, changeOrigin rewrites Host
  const echo = await (await fetch(`http://localhost:${PORT}/api/echo`, { method: "POST", body: "payload" })).json();
  assert.equal(echo.got, "payload");
  assert.equal(echo.host, `localhost:${UPSTREAM}`);

  // handshake diagnostics: the upgrade must be answered with 101
  const hs = await new Promise((resolve) => {
    const r = http.request({ host: "127.0.0.1", port: PORT, path: "/api/ws", headers: { Connection: "Upgrade", Upgrade: "websocket", "Sec-WebSocket-Version": "13", "Sec-WebSocket-Key": "dGhlIHNhbXBsZSBub25jZQ==", "Sec-WebSocket-Protocol": "chat" } });
    r.on("upgrade", (res, socket) => { socket.destroy(); resolve({ status: res.statusCode, proto: res.headers["sec-websocket-protocol"] }); });
    r.on("response", (res) => { let b = ""; res.on("data", (c) => (b += c)); res.on("end", () => resolve({ status: res.statusCode, body: b })); });
    r.on("error", (e) => resolve({ error: String(e) }));
    r.end();
  });
  assert.equal(hs.status, 101, `ws handshake through the proxy: ${JSON.stringify(hs)}`);
  // websocket tunnel with subprotocol negotiation
  const got = await new Promise((resolve, reject) => {
    const ws = new WebSocket(`ws://localhost:${PORT}/api/ws?x=1`, ["chat", "other"]);
    const msgs = [];
    const timer = setTimeout(() => reject(new Error(`ws timeout; got ${JSON.stringify(msgs)}`)), 10000);
    ws.onopen = () => ws.send("ping-me");
    ws.onmessage = (e) => { msgs.push(String(e.data)); if (msgs.length === 2) { clearTimeout(timer); resolve({ msgs, protocol: ws.protocol }); ws.close(); } };
    ws.onerror = () => { clearTimeout(timer); reject(new Error("ws error")); };
  });
  assert.deepEqual(got.msgs, ["hello from upstream /api/ws?x=1", "echo:ping-me"]);
  assert.equal(got.protocol, "chat", "upstream-selected subprotocol is relayed");
  // wss:// / https:// targets: `secure: false` accepts the self-signed upstream over
  // HTTP and WebSocket; the default verifies and refuses it with 502.
  const tlsHttp = await fetch(`http://localhost:${PORT}/tls/api/x`);
  assert.equal(tlsHttp.status, 200, "https target with secure:false proxies");
  assert.equal(tlsHttp.headers.get("x-upstream"), "tls");
  assert.equal(await tlsHttp.text(), "tls:/tls/api/x");
  const tlsWs = await new Promise((resolve, reject) => {
    const ws = new WebSocket(`ws://localhost:${PORT}/tls/api/ws`);
    const msgs = [];
    const timer = setTimeout(() => reject(new Error(`wss tunnel timeout; got ${JSON.stringify(msgs)}`)), 10000);
    ws.onopen = () => ws.send("over-tls");
    ws.onmessage = (e) => { msgs.push(String(e.data)); if (msgs.length === 2) { clearTimeout(timer); resolve(msgs); ws.close(); } };
    ws.onerror = () => { clearTimeout(timer); reject(new Error("wss tunnel error")); };
  });
  assert.deepEqual(tlsWs, ["hello from upstream /tls/api/ws", "echo:over-tls"]);
  const strict = await fetch(`http://localhost:${PORT}/strict/api/x`);
  assert.equal(strict.status, 502, "an unverifiable certificate is refused by default");
  const strictWs = await new Promise((resolve) => {
    const r = http.request({ host: "127.0.0.1", port: PORT, path: "/strict/api/ws", headers: { Connection: "Upgrade", Upgrade: "websocket", "Sec-WebSocket-Version": "13", "Sec-WebSocket-Key": "dGhlIHNhbXBsZSBub25jZQ==" } });
    r.on("upgrade", (res, socket) => { socket.destroy(); resolve(res.statusCode); });
    r.on("response", (res) => { let b = ""; res.on("data", (c) => (b += c)); res.on("end", () => resolve({ status: res.statusCode, body: b })); });
    r.on("error", (e) => resolve({ error: String(e) }));
    r.end();
  });
  assert.equal(strictWs.status, 502, `an unverifiable wss upstream is refused by default: ${JSON.stringify(strictWs)}`);
  assert.match(strictWs.body, /invalid peer certificate|UnknownIssuer/, `refused by TLS verification, not by something else: ${strictWs.body}`);

  // Without changeOrigin the browser's Host reaches the upstream (http-proxy), and
  // rewriteWsOrigin swaps the Origin for the target's origin (Vite).
  const kept = await new Promise((resolve) => {
    const r = http.request({ host: "127.0.0.1", port: PORT, path: "/keep/ws", headers: { Connection: "Upgrade", Upgrade: "websocket", "Sec-WebSocket-Version": "13", "Sec-WebSocket-Key": "dGhlIHNhbXBsZSBub25jZQ==", Origin: `http://localhost:${PORT}`, Host: `localhost:${PORT}` } });
    r.on("upgrade", (res, socket, head) => {
      let buf = Buffer.from(head);
      const finish = () => { const frames = wsParse(buf).filter((f) => f.op === 1).map((f) => f.payload.toString()); if (frames.length >= 2) { socket.destroy(); resolve(frames[1]); } };
      finish();
      socket.on("data", (d) => { buf = Buffer.concat([buf, d]); finish(); });
      setTimeout(() => { socket.destroy(); resolve("timeout: " + buf.toString("utf8")); }, 5000);
    });
    r.on("response", (res) => resolve("status " + res.statusCode));
    r.on("error", (e) => resolve(String(e)));
    r.end();
  });
  assert.equal(kept, `host=localhost:${PORT} origin=http://localhost:${UPSTREAM}`);

  console.log("PROXY-WS-STREAM E2E PASSED");
} catch (err) {
  failed = true;
  console.error("PROXY-WS-STREAM E2E FAILED:", err.message);
  try { console.error(fs.readFileSync(logPath, "utf8").split("\n").slice(-15).join("\n")); } catch {}
} finally {
  srv.kill("SIGKILL");
  upstream.close();
  tlsUpstream.close();
  await sleep(300);
  // The SIGKILLed server's node children flush for a beat and race the removal
  // (ENOTEMPTY); retry like proxy-target-forms.mjs / start-cloudflare-dev.mjs.
  for (let i = 0; ; i++) {
    try { fs.rmSync(app, { recursive: true, force: true }); break; }
    catch { if (i >= 20) break; Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100); }
  }
}
process.exit(failed ? 1 : 0);
