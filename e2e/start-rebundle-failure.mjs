// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// A Start dev rebundle that fails (here an edit importing a package that is
// not installed) names its cause in the overlay and the dev server log, never
// a generic "client bundle failed" or "change outside every served graph".
// Once the import is fixed, the next bundle clears the overlay.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "..");
const app = path.join(repo, "e2e", "fixtures", "start-app");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 5404);
const DIALOG = 'div[role="dialog"][aria-label="Build error"]';
const MISSING = "oj-rebundle-missing-dep";

if (!fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start"))) {
  console.log("SKIP start rebundle failure: fixture deps not installed");
  process.exit(0);
}
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

const aboutFile = path.join(app, "src", "routes", "about.tsx");
const originalAbout = fs.readFileSync(aboutFile, "utf8");
// The marker is set after hydration, so it shows what the client bundle runs.
const about = (marker, extra = "") => `import { createRoute } from "@tanstack/react-router";${extra}
import { useEffect, useState } from "react";

import { rootRoute } from "./__root";

export const aboutRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/about",
  component: About,
});

function About() {
  const [mark, setMark] = useState("pending");
  useEffect(() => setMark(${JSON.stringify(marker)}), []);
  return (
    <main>
      <h1 className="fixture-heading">about-page-marker</h1>
      <p data-mark="client">{mark}</p>
    </main>
  );
}
`;

let srv;
let browser;
let log = "";
try {
  fs.writeFileSync(aboutFile, about("client-one"));
  fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
  srv = spawn(oj, ["dev", app, "--port", String(PORT)], {
    stdio: ["ignore", "pipe", "pipe"],
    // Ungated hot mode: an editor flush gate would hold the recovery reload.
    env: { ...process.env, LOVABLE_DEV_SERVER: "0", OJ_HMR_GATE: "0" },
  });
  srv.stdout.on("data", (d) => (log += d));
  srv.stderr.on("data", (d) => (log += d));
  await waitUp(`http://localhost:${PORT}/`);

  browser = await chromium.launch();
  const page = await browser.newPage();
  await page.goto(`http://localhost:${PORT}/about`, { waitUntil: "load", timeout: 30000 });
  await page.waitForSelector('[data-mark="client"]:text("client-one")', { timeout: 30000 });

  const logFrom = log.length;
  fs.writeFileSync(aboutFile, about("client-two", `\nimport ${JSON.stringify(MISSING)};`));
  const after = () => log.slice(logFrom);
  const deadline = Date.now() + 30000;
  while (!after().includes("client bundle failed") && Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 100));
  }
  must(
    new RegExp(`client bundle failed.*failed to resolve import "${MISSING}"`).test(after()),
    `the dev server log does not name the bundler's error:\n${after()}`,
  );
  must(
    !after().includes("change outside every served graph"),
    `the failed rebundle was logged as a change outside every served graph:\n${after()}`,
  );
  const named = await page.waitForSelector(`${DIALOG}:has-text("${MISSING}")`, { timeout: 30000 }).then(
    () => true,
    () => false,
  );
  if (!named) {
    const text = await page
      .locator(DIALOG)
      .innerText({ timeout: 5000 })
      .catch(() => "(no overlay)");
    throw new Error(`the overlay does not name the bundler's error:\n${text}`);
  }
  console.log("start dev: a failed rebundle names its error in the overlay and the log");

  fs.writeFileSync(aboutFile, about("client-three"));
  await page.waitForSelector(DIALOG, { state: "detached", timeout: 30000 });
  console.log("start dev: with the import fixed, the next bundle clears the overlay");
  console.log("\nSTART REBUNDLE FAILURE PASSED");
} catch (e) {
  console.error("\nSTART REBUNDLE FAILURE FAILED:", e.message);
  if (log) console.error(`--- server log tail ---\n${log.slice(-2000)}`);
  process.exitCode = 1;
} finally {
  await browser?.close();
  srv?.kill("SIGKILL");
  fs.writeFileSync(aboutFile, originalAbout);
}
