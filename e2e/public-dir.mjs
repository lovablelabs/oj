// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

// publicDir in dev: files under it are served byte-for-byte (Vite's public
// middleware runs before any transform), an explicit asset query still yields
// a module, `publicDir: "<dir>"` relocates it and `publicDir: false` turns it
// off. Run with a built target/debug/oj.
import { spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const SW = `self.addEventListener("install", () => {});\nconst mode = import.meta.env;\nimport "./nope.js";\n// raw public bytes\n`;
const CSS = `body { color: red; }  /* raw public bytes */\n`;
const JSON_SRC = `{ "raw": true,   "spaces": "kept" }\n`;
const HTML = `<html><head><title>public</title></head><body>public page</body></html>\n`;

function scaffold(publicDirName, config) {
  const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-public-dir-"));
  fs.mkdirSync(path.join(app, "src"), { recursive: true });
  fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "public-dir", type: "module" }));
  if (config) fs.writeFileSync(path.join(app, "oj.config.ts"), config);
  fs.writeFileSync(
    path.join(app, "index.html"),
    `<!doctype html><html><head></head><body><script type="module" src="/src/main.js"></script></body></html>`,
  );
  fs.writeFileSync(path.join(app, "src", "main.js"), `import logo from "/logo.svg?url";\nwindow.__LOGO = logo;\n`);
  const pub = path.join(app, publicDirName);
  fs.mkdirSync(pub, { recursive: true });
  fs.writeFileSync(path.join(pub, "sw.js"), SW);
  fs.writeFileSync(path.join(pub, "style.css"), CSS);
  fs.writeFileSync(path.join(pub, "data.json"), JSON_SRC);
  fs.writeFileSync(path.join(pub, "page.html"), HTML);
  fs.writeFileSync(path.join(pub, "logo.svg"), `<svg xmlns="http://www.w3.org/2000/svg"/>`);
  return app;
}

async function withServer(app, port, fn) {
  // Own process group (setsid), so the teardown can SIGKILL the server AND
  // the one-shot children it spawned at boot (the dep optimizer engine job).
  const srv = spawn(oj, ["dev", app, "--port", String(port)], { stdio: ["ignore", "pipe", "pipe"], detached: true });
  let log = "";
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  try {
    let up = false;
    for (let i = 0; i < 100; i++) {
      try {
        if ((await fetch(`http://localhost:${port}/`)).ok) {
          up = true;
          break;
        }
      } catch {}
      await sleep(200);
    }
    if (!up) throw new Error(`server did not start on ${port}:\n${log}`);
    await fn(`http://localhost:${port}`);
  } finally {
    // SIGKILL the whole group like the run.mjs harness kills the server (no
    // graceful shutdown = no engine code-cache flush writers), and WAIT for
    // the exit so the caller's teardown rm never races the dying process.
    // The group kill covers the boot-time engine-job child too: killed only
    // through its parent, a child still loading the binary missed the ppid
    // reaper and ran its whole job, writing code caches into the app dir the
    // rm was deleting (the recurring ENOTEMPTY); the rm retries stay as a
    // margin for any other straggler.
    try {
      process.kill(-srv.pid, "SIGKILL");
    } catch {
      srv.kill("SIGKILL");
    }
    await new Promise((resolve) => srv.once("exit", resolve));
  }
}

let failed = false;
function check(label, ok, detail) {
  if (!ok) {
    failed = true;
    console.error(`FAIL ${label}: ${detail}`);
  } else {
    console.log(`ok   ${label}`);
  }
}

async function get(url, headers = {}) {
  const r = await fetch(url, { headers });
  return { status: r.status, type: r.headers.get("content-type") ?? "", body: await r.text() };
}

// 1. Default public/: verbatim bytes for js, css, json and html.
{
  const app = scaffold("public");
  try {
    await withServer(app, 6301, async (origin) => {
      const sw = await get(`${origin}/sw.js`, { "sec-fetch-dest": "script" });
      check("public sw.js served verbatim", sw.status === 200 && sw.body === SW, JSON.stringify(sw));
      check("public sw.js has a javascript content type", /javascript/.test(sw.type), sw.type);
      const css = await get(`${origin}/style.css`, { "sec-fetch-dest": "style" });
      check("public style.css served verbatim", css.status === 200 && css.body === CSS, JSON.stringify(css));
      const json = await get(`${origin}/data.json`);
      check("public data.json served verbatim", json.status === 200 && json.body === JSON_SRC, JSON.stringify(json));
      const html = await get(`${origin}/page.html`);
      check("public page.html served without dev script injection", html.status === 200 && html.body === HTML, JSON.stringify(html));
      const mod = await get(`${origin}/logo.svg?url`);
      check("?url on a public asset is still a module", mod.status === 200 && /export default "\/logo\.svg"/.test(mod.body), JSON.stringify(mod));
    });
  } finally {
    fs.rmSync(app, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
}

// 2. publicDir: "static" relocates the directory.
{
  const app = scaffold("static", `export default { publicDir: "static" };\n`);
  try {
    await withServer(app, 6302, async (origin) => {
      const sw = await get(`${origin}/sw.js`, { "sec-fetch-dest": "script" });
      check("custom publicDir file served verbatim", sw.status === 200 && sw.body === SW, JSON.stringify(sw));
    });
  } finally {
    fs.rmSync(app, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
}

// 3. publicDir: false disables it (files under public/ are not reachable).
{
  const app = scaffold("public", `export default { publicDir: false };\n`);
  try {
    await withServer(app, 6303, async (origin) => {
      const sw = await get(`${origin}/sw.js`);
      check("publicDir false hides public/sw.js", sw.status === 404, JSON.stringify(sw));
      const main = await get(`${origin}/src/main.js`);
      check("root modules still compile with publicDir false", main.status === 200 && /__LOGO/.test(main.body), JSON.stringify(main));
    });
  } finally {
    fs.rmSync(app, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  }
}

if (failed) process.exit(1);
console.log("PUBLIC-DIR E2E PASSED");
