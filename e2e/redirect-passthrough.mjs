// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// A sign-in and sign-out that rely on browser redirects, in a real browser
// through `oj dev`, for a plain SPA and a TanStack Start app.
//
// The rule (Vite): a plugin's `configureServer` middleware writes its reply
// inside Vite's own connect app, so a redirect and its `Set-Cookie` headers
// reach the browser untouched (and `server.proxy` keeps http-proxy's
// `followRedirects: false`). The browser then follows the redirect itself:
// its URL changes and the cookies apply. oj's loopback hop to the plugin
// middleware followed redirects on the server, so the browser stayed on
// `/auth/sign-out` showing the destination's page and never saw the
// cleared session cookie.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.join(here, "..");
const fixture = path.join(here, "fixtures", "start-app");
const oj = path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 3116);

let pw = null;
try {
  pw = await import("playwright");
} catch {}
if (!pw) {
  console.log("SKIP redirect passthrough: playwright not installed");
  process.exit(0);
}
const startInstalled = fs.existsSync(path.join(fixture, "node_modules", "@tanstack", "react-start"));

execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });
const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

// The auth middleware both apps install: `/login` sets the session and
// redirects home; `/sign-out` is a page whose form POSTs `/auth/sign-out`,
// which clears the session and 303s to an absolute URL built from the
// request's Host (what auth libraries do); `/signed-out` echoes the Cookie
// header the browser sent it.
const AUTH_PLUGIN = `{
    name: "redirect-auth",
    configureServer(server) {
      server.middlewares.use((req, res, next) => {
        const url = req.url.split("?")[0];
        if (url === "/login") {
          res.writeHead(302, { Location: "/", "Set-Cookie": "session=abc; Path=/" });
          return res.end();
        }
        if (url === "/sign-out") {
          res.setHeader("content-type", "text/html");
          return res.end('<form method="post" action="/auth/sign-out"><button id="go">Sign out</button></form>');
        }
        if (url === "/auth/sign-out" && req.method === "POST") {
          res.writeHead(303, {
            Location: "http://" + req.headers.host + "/signed-out",
            "Set-Cookie": ["session=; Max-Age=0; Path=/", "logout=done; Path=/"],
          });
          return res.end();
        }
        if (url === "/signed-out") {
          res.setHeader("content-type", "text/html");
          return res.end('<p id="done">signed-out</p><p id="cookie">' + (req.headers.cookie || "") + "</p>");
        }
        next();
      });
    },
  }`;

function spaApp() {
  const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-redirect-spa-"));
  fs.writeFileSync(path.join(app, "package.json"), JSON.stringify({ name: "redirect-spa", type: "module" }));
  fs.writeFileSync(path.join(app, "vite.config.mjs"), `export default { plugins: [${AUTH_PLUGIN}] };\n`);
  fs.writeFileSync(path.join(app, "main.js"), 'document.body.dataset.ready = "1";\n');
  fs.writeFileSync(
    path.join(app, "index.html"),
    '<!doctype html><html><head><title>home</title></head><body><script type="module" src="/main.js"></script></body></html>',
  );
  return app;
}

function startApp() {
  const app = fs.mkdtempSync(path.join(os.tmpdir(), "oj-redirect-start-"));
  fs.cpSync(fixture, app, { recursive: true, filter: (src) => !/[\\/](node_modules|\.oj-cache)$/.test(src) });
  fs.symlinkSync(path.join(fixture, "node_modules"), path.join(app, "node_modules"), "dir");
  const cfg = path.join(app, "vite.config.ts");
  const src = fs.readFileSync(cfg, "utf8");
  must(src.includes("plugins: ["), "fixture vite.config.ts changed shape");
  fs.writeFileSync(cfg, src.replace("plugins: [", `plugins: [\n    ${AUTH_PLUGIN},`));
  return app;
}

async function signInAndOut(label, base) {
  const browser = await pw.chromium.launch();
  try {
    const context = await browser.newContext();
    const page = await context.newPage();
    const session = async () => (await context.cookies(base)).find((c) => c.name === "session")?.value;

    // A GET redirect with a cookie: the browser lands home, signed in.
    await page.goto(`${base}/login`);
    must(new URL(page.url()).pathname === "/", `${label}: /login left the browser at ${page.url()}`);
    must((await session()) === "abc", `${label}: the login cookie never reached the browser`);

    // A POST answered with a 303: the browser follows it, its URL changes,
    // the session cookie is cleared before the destination request.
    await page.goto(`${base}/sign-out`);
    await Promise.all([page.waitForURL(`${base}/signed-out`, { timeout: 15000 }).catch(() => {}), page.click("#go")]);
    must(
      page.url() === `${base}/signed-out`,
      `${label}: after sign-out the browser shows ${page.url()} (the server followed the redirect)`,
    );
    must((await page.locator("#done").textContent()) === "signed-out", `${label}: the destination page did not render`);
    const sent = await page.locator("#cookie").textContent();
    must(!sent.includes("session=abc"), `${label}: the destination still received the session cookie: ${sent}`);
    must(sent.includes("logout=done"), `${label}: the sign-out cookie never applied: ${sent}`);
    must((await session()) === undefined, `${label}: the browser kept the session cookie`);
    console.log(`${label}: login and sign-out redirects reach the browser with their cookies`);
  } finally {
    await browser.close();
  }
}

async function served(label, app) {
  const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "ignore", "pipe"], detached: true });
  let stderr = "";
  srv.stderr.on("data", (d) => (stderr += d));
  const exited = new Promise((r) => srv.once("exit", r));
  const base = `http://localhost:${PORT}`;
  try {
    const up = await waitUp(`${base}/`, { until: () => true }).then(
      () => true,
      () => false,
    );
    must(up, `${label}: server on :${PORT} did not start\n${stderr.slice(-2000)}`);
    // The plugin middleware comes up with the plugin host; wait for it.
    const deadline = Date.now() + 30000;
    while (
      (
        await fetch(`${base}/sign-out`).then(
          (r) => r.text(),
          () => "",
        )
      ).indexOf('id="go"') === -1
    ) {
      must(Date.now() < deadline, `${label}: plugin middleware never answered\n${stderr.slice(-2000)}`);
      await new Promise((r) => setTimeout(r, 200));
    }
    await signInAndOut(label, base);
  } catch (e) {
    throw new Error(`${e.message}\n--- oj stderr ---\n${stderr.slice(-2000)}`);
  } finally {
    try {
      process.kill(-srv.pid, "SIGKILL");
    } catch {}
    await exited;
    fs.rmSync(app, { recursive: true, force: true });
  }
}

try {
  await served("spa dev", spaApp());
  if (startInstalled) await served("start dev", startApp());
  else console.log("SKIP start dev half: fixture deps not installed");
  console.log("\nREDIRECT PASSTHROUGH PASSED");
} catch (e) {
  console.error(`\nREDIRECT PASSTHROUGH FAILED: ${e.message}`);
  process.exitCode = 1;
}
