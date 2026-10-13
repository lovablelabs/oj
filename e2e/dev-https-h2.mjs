// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// `server.https` serves TLS with ALPN h2 + http/1.1, as Vite's
// resolveHttpServer uses node's http2.createSecureServer (allowHTTP1) when
// https is configured. The config reads the key with readFileSync (a Buffer
// across the plugin-host bridge, the way user configs and basic-ssl set it)
// and names the cert by path (Vite's readFileIfExists string shape). A
// browser drives an edit-reload roundtrip on top, so the HMR WebSocket path
// over TLS is covered too.
import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import http2 from "node:http2";
import https from "node:https";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

// Self-signed certificate: node's tls honors this for every client below.
process.env.NODE_TLS_REJECT_UNAUTHORIZED = "0";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const PORT = 5497;

try {
  execSync("openssl version", { stdio: "ignore" });
} catch {
  console.log("SKIP dev-https-h2: openssl not available to mint a test certificate");
  process.exit(0);
}

if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

// A survivor from a crashed run would answer the port and shadow this server.
try {
  execSync(`lsof -ti:${PORT} -sTCP:LISTEN | xargs kill -9`, { stdio: "ignore" });
} catch {}

const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-https-h2-"));
fs.mkdirSync(path.join(app, "src"), { recursive: true });
execSync(`openssl req -x509 -newkey rsa:2048 -keyout key.pem -out cert.pem -days 2 -nodes -subj "/CN=localhost"`, {
  cwd: app,
  stdio: "ignore",
});
fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "https-h2", version: "1.0.0" }));
fs.writeFileSync(path.join(app, "src", "main.js"), `document.title = "tls-v1"; window.__READY = true;\n`);
fs.writeFileSync(
  path.join(app, "index.html"),
  `<!doctype html><html><head><title>t</title></head><body><script type="module" src="/src/main.js"></script></body></html>`,
);
fs.writeFileSync(
  path.join(app, "vite.config.ts"),
  `import { readFileSync } from "node:fs";
import { defineConfig } from "vite";

export default defineConfig({
  server: {
    https: {
      key: readFileSync(new URL("./key.pem", import.meta.url)),
      // A path string: Vite's readFileIfExists shape (resolved against the
      // app root), covered next to the Buffer shape above.
      cert: "./cert.pem",
    },
  },
});
`,
);

// The config imports "vite" for defineConfig; resolve it from the start fixture.
fs.symlinkSync(path.join(here, "fixtures", "start-app", "node_modules"), path.join(app, "node_modules"), "dir");

const base = `https://localhost:${PORT}`;

async function h2Get(pathname) {
  return new Promise((resolve, reject) => {
    const session = http2.connect(base, { rejectUnauthorized: false });
    session.on("error", reject);
    session.on("connect", () => {
      const alpn = session.alpnProtocol;
      const req = session.request({ ":path": pathname });
      let body = "";
      req.setEncoding("utf8");
      let status = 0;
      req.on("response", (h) => (status = h[":status"]));
      req.on("data", (c) => (body += c));
      req.on("end", () => {
        session.close();
        resolve({ status, body, alpn });
      });
      req.on("error", reject);
      req.end();
    });
  });
}

function h1Get(pathname) {
  return new Promise((resolve, reject) => {
    const req = https.request(
      `${base}${pathname}`,
      { rejectUnauthorized: false, ALPNProtocols: ["http/1.1"] },
      (res) => {
        // Read before end: the socket detaches once the stream closes.
        const alpn = res.socket?.alpnProtocol;
        let body = "";
        res.setEncoding("utf8");
        res.on("data", (c) => (body += c));
        res.on("end", () => resolve({ status: res.statusCode, body, alpn }));
      },
    );
    req.on("error", reject);
    req.end();
  });
}

let failed = false;
let log = "";
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], {
  stdio: ["ignore", "pipe", "pipe"],
});
srv.stdout.on("data", (c) => (log += c));
srv.stderr.on("data", (c) => (log += c));
let browser;
try {
  // waitUp speaks plain http; this server only answers TLS, so poll h1 directly.
  const t = Date.now();
  for (;;) {
    try {
      const r = await h1Get("/");
      if (r.status < 500) break;
    } catch {}
    if (srv.exitCode !== null) throw new Error(`oj exited ${srv.exitCode}:\n${log.slice(-2000)}`);
    if (Date.now() - t > 120000) throw new Error(`server never answered TLS:\n${log.slice(-2000)}`);
    await sleep(200);
  }

  if (!log.includes(`https://localhost:${PORT}/`)) {
    throw new Error(`the printed url is not https:\n${log.slice(-1500)}`);
  }

  const doc = await h2Get("/");
  if (doc.alpn !== "h2") throw new Error(`ALPN negotiated ${doc.alpn}, not h2`);
  if (doc.status !== 200 || !doc.body.includes("/src/main.js")) {
    throw new Error(`document over h2: ${doc.status}\n${doc.body.slice(0, 300)}`);
  }
  const mod = await h2Get("/src/main.js");
  if (mod.status !== 200 || !mod.body.includes("tls-v1")) {
    throw new Error(`module over h2: ${mod.status}`);
  }
  console.log("https-h2: document and module served over ALPN h2");

  const h1 = await h1Get("/");
  if (h1.alpn !== "http/1.1" || h1.status !== 200) {
    throw new Error(`http/1.1 fallback: alpn ${h1.alpn}, status ${h1.status}`);
  }
  console.log("https-h2: http/1.1-only client still served");

  // The full client pipeline over TLS: page loads, the HMR socket connects
  // (wss, from location.protocol), an edit reloads onto the new module.
  browser = await chromium.launch();
  const page = await browser.newPage({ ignoreHTTPSErrors: true });
  page.on("pageerror", (e) => console.error("pageerror:", String(e).slice(0, 200)));
  page.on("requestfailed", (r) => console.error("reqfail:", r.url().slice(0, 120), r.failure()?.errorText));
  await page.goto(`${base}/`, { timeout: 30000 });
  await page.waitForFunction(() => window.__READY === true, undefined, { timeout: 15000 });
  fs.writeFileSync(path.join(app, "src", "main.js"), `document.title = "tls-v2"; window.__READY = true;\n`);
  await page.waitForFunction(() => document.title === "tls-v2", undefined, { timeout: 15000 });
  console.log("https-h2: edit-reload roundtrip over TLS (wss) works");

  console.log("DEV-HTTPS-H2 E2E PASSED");
} catch (err) {
  failed = true;
  console.error("DEV-HTTPS-H2 E2E FAILED:", err.message);
  console.error(log.slice(-1500));
} finally {
  if (browser) await browser.close();
  srv.kill("SIGKILL");
  await sleep(300);
  fs.rmSync(app, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
