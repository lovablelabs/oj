// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Start dev shows a failed client bundle the way Vite's bundled dev does,
// instead of a blank page: the dev server stays up when the boot bundle
// fails, every page gets the error overlay (the failure is replayed to each
// page that connects, Vite's lastBuildError), the fix reloads the page into a
// working app, and a later failure shows the overlay without reloading onto
// the stale bundle. The broken import is client-only (SSR renders the page),
// the shape where the page used to hydrate to nothing. Which failures throw,
// the payload shape and the socket protocol are covered in Rust
// (oj_server/tests/start_client_errors.rs, start_dev.rs); this checks the
// browser wiring, and that `oj build` fails on the same import.

import { spawn, spawnSync, execSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "..");
const app = path.join(repo, "e2e", "fixtures", "start-app");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 5403);
const DIALOG = 'div[role="dialog"][aria-label="Build error"]';

if (!fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start"))) {
  console.log("SKIP start error overlay: fixture deps not installed");
  process.exit(0);
}
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

// A killed server's bundler child can still be writing the cache for a moment.
const rmCache = () =>
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

const aboutFile = path.join(app, "src", "routes", "about.tsx");
const originalAbout = fs.readFileSync(aboutFile, "utf8");
const about = (spec) => `import { createRoute } from "@tanstack/react-router";
import { useEffect, useState } from "react";

import { rootRoute } from "./__root";

export const aboutRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/about",
  component: About,
});

function About() {
  const [loaded, setLoaded] = useState("pending");
  useEffect(() => {
    import(${JSON.stringify(spec)}).then(() => setLoaded("client-import-ok"));
  }, []);
  return (
    <main>
      <h1 className="fixture-heading">about-page-marker</h1>
      <p data-mark="client">{loaded}</p>
    </main>
  );
}
`;
const BROKEN = about("no-such-dep-typo");
// The fixture's config with Vite's `server.hmr.overlay: false`.
const overlayOffConfig = path.join(app, "vite.overlay-off.config.ts");
const FIXED = about("../lib/format");

let srv;
let browser;
let vendor;
let log = "";
try {
  fs.writeFileSync(aboutFile, BROKEN);
  rmCache();
  srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  await waitUp(`http://localhost:${PORT}/`);

  browser = await chromium.launch();
  const page = await browser.newPage();
  await page.goto(`http://localhost:${PORT}/about`, { waitUntil: "load", timeout: 30000 });
  await page.waitForSelector(DIALOG, { timeout: 15000 });
  const text = await page.locator(DIALOG).innerText();
  must(
    text.includes('failed to resolve import "no-such-dep-typo"') && text.includes("about.tsx"),
    `the overlay does not name the unresolved import and its file:\n${text}`,
  );
  must(
    (await page.locator("h1.fixture-heading").innerText()).includes("about-page-marker"),
    "SSR content is missing under the overlay",
  );
  console.log("start dev: a failed boot bundle shows the error overlay");

  // The fix rebundles and reloads into a working client.
  fs.writeFileSync(aboutFile, FIXED);
  await page.waitForSelector('[data-mark="client"]:text("client-import-ok")', { timeout: 30000 });
  must((await page.locator(DIALOG).count()) === 0, "the overlay survived the fix");
  console.log("start dev: the fix reloads into a hydrated page");

  // A later failure overlays the running page; it never reloads onto the stale bundle.
  await page.evaluate(() => (window.__ojNoReload = true));
  fs.writeFileSync(aboutFile, BROKEN);
  await page.waitForSelector(DIALOG, { timeout: 30000 });
  must(
    await page.evaluate(() => window.__ojNoReload === true),
    "a failed rebundle reloaded the page instead of overlaying it",
  );
  console.log("start dev: a failed rebundle overlays the page without a reload");
  srv.kill("SIGKILL");
  srv = null;

  // `server.hmr.overlay: false`: Vite's client only logs the error frame.
  fs.writeFileSync(
    overlayOffConfig,
    'import base from "./vite.config";\n' +
      "export default { ...base, server: { ...base.server, hmr: { overlay: false } } };\n",
  );
  rmCache();
  srv = spawn(oj, ["dev", app, "--port", String(PORT), "--config", overlayOffConfig], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  await waitUp(`http://localhost:${PORT}/`);
  const quiet = await browser.newPage();
  const logged = new Promise((resolve) =>
    quiet.on("console", (m) => m.type() === "error" && m.text().includes("[oj] build error") && resolve(m.text())),
  );
  await quiet.goto(`http://localhost:${PORT}/about`, { waitUntil: "load", timeout: 30000 });
  const message = await Promise.race([logged, new Promise((r) => setTimeout(() => r(null), 15000))]);
  must(message?.includes('failed to resolve import "no-such-dep-typo"'), `no build error was logged: ${message}`);
  must((await quiet.locator(DIALOG).count()) === 0, "the overlay mounted with server.hmr.overlay false");
  console.log("start dev: server.hmr.overlay false logs the build error without the overlay");
  srv.kill("SIGKILL");
  srv = null;

  // A rebundle that fails before its build call (here the rolldown it loads
  // went missing mid-session) shows and logs why, like any failed build. It
  // used to show a bare "client bundle failed", log nothing, and claim the
  // edit was "outside every served graph".
  fs.writeFileSync(aboutFile, FIXED);
  vendor = fs.mkdtempSync(path.join(os.tmpdir(), "oj-overlay-vendor-"));
  const vendored = path.join(vendor, "node_modules", "rolldown");
  fs.mkdirSync(path.dirname(vendored), { recursive: true });
  fs.writeFileSync(path.join(vendor, "package.json"), "{}");
  const linkRolldown = () => fs.symlinkSync(fs.realpathSync(path.join(app, "node_modules", "rolldown")), vendored);
  linkRolldown();
  rmCache();
  log = "";
  srv = spawn(oj, ["dev", app, "--port", String(PORT)], {
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, OJ_VENDORED_ROLLDOWN: vendor },
  });
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  await waitUp(`http://localhost:${PORT}/`);
  const early = await browser.newPage();
  await early.goto(`http://localhost:${PORT}/about`, { waitUntil: "load", timeout: 30000 });
  await early.waitForSelector('[data-mark="client"]:text("client-import-ok")', { timeout: 30000 });
  fs.rmSync(vendored);
  const mark = log.length;
  fs.writeFileSync(aboutFile, FIXED.replace("about-page-marker", "about-page-marker-2"));
  await early.waitForSelector(DIALOG, { timeout: 30000 });
  const earlyText = await early.locator(DIALOG).innerText();
  must(earlyText.includes("OJ_VENDORED_ROLLDOWN"), `the overlay does not say why the bundle failed:\n${earlyText}`);
  const after = log.slice(mark);
  must(
    /client bundle failed: .*OJ_VENDORED_ROLLDOWN/.test(after),
    `the dev server log does not say why the bundle failed:\n${after.slice(-2000)}`,
  );
  must(!after.includes("outside every served graph"), "a failed rebundle was logged as a change outside every graph");
  console.log("start dev: a rebundle failing before its build shows and logs why");

  // Rolldown back: the next edit bundles and clears the overlay.
  linkRolldown();
  fs.writeFileSync(aboutFile, FIXED);
  await early.waitForFunction((sel) => !document.querySelector(sel), DIALOG, { timeout: 30000 });
  console.log("start dev: the next edit after the fix clears the overlay");
  await browser.close();
  browser = null;
  srv.kill("SIGKILL");
  srv = null;

  // `vite build` fails on the same import (onRollupLog), never shipping it bare.
  fs.writeFileSync(aboutFile, BROKEN);
  const dist = path.join(app, "dist");
  fs.rmSync(dist, { recursive: true, force: true });
  const built = spawnSync(oj, ["build", app], { encoding: "utf8" });
  const buildLog = `${built.stdout}\n${built.stderr}`;
  fs.rmSync(dist, { recursive: true, force: true });
  must(built.status !== 0, `the build shipped an unresolved import:\n${buildLog.slice(-2000)}`);
  must(
    buildLog.includes('failed to resolve import "no-such-dep-typo"'),
    `the build failure does not name the import:\n${buildLog.slice(-2000)}`,
  );
  console.log("start build: an unresolved client import fails the build");
  console.log("\nSTART ERROR OVERLAY PASSED");
} catch (e) {
  console.error("\nSTART ERROR OVERLAY FAILED:", e.message);
  if (log) console.error(`--- server log tail ---\n${log.slice(-2000)}`);
  process.exitCode = 1;
} finally {
  await browser?.close();
  srv?.kill("SIGKILL");
  fs.writeFileSync(aboutFile, originalAbout);
  fs.rmSync(overlayOffConfig, { force: true });
  if (vendor) fs.rmSync(vendor, { recursive: true, force: true });
}
