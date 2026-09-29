// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// server.hmr options and the socket token reach the served client the way
// Vite's clientInjections fill /@vite/client:
//  - hmr.path moves the socket route and the client dial; hmr.clientPort,
//    hmr.host and hmr.protocol are honored; hmr.overlay:false suppresses the
//    error overlay (the error still logs);
//  - a browser upgrade (one carrying Origin) needs ?token=<per-process token>:
//    without it 401, with it 101; a non-browser client connects freely and
//    vite-ping is exempt;
//  - the `vite-error-overlay` custom element exists and is constructible with
//    an ErrorPayload err, even with the automatic overlay off.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = path.join(repo, "target", "debug", "oj");
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const port = Number(process.env.OJ_E2E_PORT || 5489);

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-hmropts-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "hmropts-app", version: "1.0.0" }));
fs.writeFileSync(
  path.join(app, "vite.config.mjs"),
  `export default { server: { hmr: { path: "/hmr-sock", clientPort: ${port}, host: "127.0.0.1", protocol: "ws", overlay: false } } };\n`,
);
fs.writeFileSync(path.join(app, "src", "main.js"), `document.title = "v1"; window.__READY = true;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);

// A raw upgrade request so the Origin header is under our control (the WebSocket
// constructor never lets a client set it).
function upgrade(pathname, headers = {}) {
  return new Promise((resolve, reject) => {
    const req = http.request({
      host: "localhost",
      port,
      path: pathname,
      headers: {
        Connection: "Upgrade",
        Upgrade: "websocket",
        "Sec-WebSocket-Version": "13",
        "Sec-WebSocket-Key": "dGhlIHNhbXBsZSBub25jZQ==",
        ...headers,
      },
    });
    req.on("upgrade", (res, socket) => {
      socket.destroy();
      resolve(res.statusCode);
    });
    req.on("response", (res) => {
      res.resume();
      resolve(res.statusCode);
    });
    req.on("error", reject);
    req.end();
  });
}

let failed = false;
const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: "ignore" });
try {
  await waitUp(`http://localhost:${port}/`);

  const client = await (await fetch(`http://localhost:${port}/@oj/client.js`)).text();
  assert.ok(!/__HMR_[A-Z_]+__|__WS_TOKEN__/.test(client), "every placeholder is filled");
  assert.match(client, /const hmrPath = "\/hmr-sock";/, "hmr.path reaches the client");
  assert.match(client, new RegExp(`const hmrPort = ${port};`), "hmr.clientPort reaches the client");
  assert.match(client, /const hmrHostname = "127\.0\.0\.1";/, "hmr.host reaches the client");
  assert.match(client, /const hmrProtocol = "ws";/, "hmr.protocol reaches the client");
  assert.match(client, /const enableOverlay = false;/, "hmr.overlay:false reaches the client");
  const token = client.match(/const wsToken = "([0-9a-f]{32})";/)?.[1];
  assert.ok(token, "a 32-hex per-process token is injected");

  // Token check: browser-like upgrades (Origin set) need the token; others do not.
  const origin = { Origin: `http://localhost:${port}` };
  assert.equal(await upgrade("/hmr-sock", origin), 401, "browser upgrade without token is refused");
  assert.equal(await upgrade("/hmr-sock?token=nope", origin), 401, "browser upgrade with a wrong token is refused");
  assert.equal(await upgrade(`/hmr-sock?token=${token}`, origin), 101, "browser upgrade with the token opens");
  assert.equal(await upgrade("/__ws", origin), 401, "the default path is guarded too");
  assert.equal(await upgrade("/__ws"), 101, "a non-browser client (no Origin) connects without a token");
  assert.equal(await upgrade("/", { ...origin, "Sec-WebSocket-Protocol": "vite-ping" }), 101, "vite-ping is exempt");
  assert.equal(
    await upgrade("/", { ...origin, "Sec-WebSocket-Protocol": "vite-hmr" }),
    401,
    "a vite-hmr browser dial needs the token",
  );
  assert.equal(await upgrade(`/?token=${token}`, { ...origin, "Sec-WebSocket-Protocol": "vite-hmr" }), 101);

  const browser = await chromium.launch();
  const page = await browser.newPage();
  const logs = [];
  page.on("console", (m) => logs.push(m.text()));
  try {
    await page.goto(`http://localhost:${port}/`, { timeout: 30000 });
    await page.waitForFunction(() => window.__READY === true, { timeout: 10000 });
    // HMR flows over the configured path.
    fs.writeFileSync(path.join(app, "src", "main.js"), `document.title = "v2"; window.__READY = true;\n`);
    await page.waitForFunction(() => document.title === "v2", { timeout: 10000 });

    // overlay:false: a compile error logs but shows no dialog.
    fs.writeFileSync(path.join(app, "src", "main.js"), `document.title = "v3"; const = ;\n`);
    await page
      .waitForFunction(() => performance.getEntriesByType("resource").length >= 0, { timeout: 1000 })
      .catch(() => {});
    await settles(() => logs.some((l) => l.includes("Internal Server Error")));
    assert.ok(
      logs.some((l) => l.includes("Internal Server Error")),
      `error logged to the console: ${logs.join(" | ")}`,
    );
    assert.equal(await page.locator('[role="dialog"]').count(), 0, "no overlay when hmr.overlay is false");

    // The custom element is registered and usable by framework runtimes.
    const shown = await page.evaluate(() => {
      const El = customElements.get("vite-error-overlay");
      if (!El) return "missing";
      const el = new El({ message: "boom from runtime", stack: "at x.js:1:1", plugin: "runtime" });
      document.body.appendChild(el);
      const dialog = el.shadowRoot.querySelector('[role="dialog"]');
      const text = dialog ? dialog.textContent : "";
      el.close();
      return { text, gone: !document.querySelector("vite-error-overlay") };
    });
    assert.notEqual(shown, "missing", "vite-error-overlay is defined");
    assert.match(shown.text, /boom from runtime/);
    assert.ok(shown.gone, "close() removes the element");
  } finally {
    await browser.close();
  }
  console.log("HMR-CLIENT-OPTIONS E2E PASSED");
} catch (err) {
  failed = true;
  console.error("HMR-CLIENT-OPTIONS E2E FAILED:", err.message);
} finally {
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
