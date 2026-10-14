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
const FIXED = about("../lib/format");

let srv;
let browser;
let log = "";
try {
  fs.writeFileSync(aboutFile, BROKEN);
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
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
  await browser.close();
  browser = null;
  srv.kill("SIGKILL");
  srv = null;

  // `vite build` fails on the same import (onRollupLog), never shipping it bare.
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
}
