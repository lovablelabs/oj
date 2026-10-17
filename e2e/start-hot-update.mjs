// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// Start dev applies an edit in place, as Vite's bundled dev does: the client
// bundler's engine turns the save into a patch for the page, React Fast
// Refresh swaps the component, and the page keeps its state (no reload).
// An edit no module accepts reloads the page into the fresh bundle, and so
// does a manual reload right after a hot update: the document waits for an
// output that carries it. The patch protocol and the reload decision are
// covered in Rust (start_hot.rs, start_dev.rs); this checks the browser
// wiring end to end.

import { spawn, execSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { settles, waitUp } from "./util.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, "..");
const app = path.join(repo, "e2e", "fixtures", "start-app");
const oj = process.env.OJ_BIN ?? path.join(repo, "target", "debug", "oj");
const PORT = Number(process.env.OJ_E2E_PORT || 5404);

if (!fs.existsSync(path.join(app, "node_modules", "@tanstack", "react-start"))) {
  console.log("SKIP start hot update: fixture deps not installed");
  process.exit(0);
}
const { chromium } = createRequire(path.join(here, "x.js"))("playwright");
if (!process.env.OJ_BIN) execSync("cargo build -p oj", { cwd: repo, stdio: "inherit" });

const must = (cond, msg) => {
  if (!cond) throw new Error(msg);
};

const widget = path.join(app, "src", "widgets", "glob-widget.jsx");
const format = path.join(app, "src", "lib", "format.ts");
const originals = new Map([widget, format].map((f) => [f, fs.readFileSync(f, "utf8")]));
const restore = () => {
  for (const [f, src] of originals) fs.writeFileSync(f, src);
};
process.on("exit", restore);

fs.rmSync(path.join(app, ".oj-cache"), { recursive: true, force: true });
let log = "";
const srv = spawn(oj, ["dev", app, "--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
srv.stdout.on("data", (d) => (log += d));
srv.stderr.on("data", (d) => (log += d));

const browser = await chromium.launch();
let failed = false;
try {
  await waitUp(`http://localhost:${PORT}/`, { proc: srv });
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  await page.goto(`http://localhost:${PORT}/`);
  await page.waitForSelector('[data-testid="client-mounted"]', { timeout: 60000 });
  await page.click('[data-testid="counter"]');
  await page.waitForSelector('[data-testid="counter"]:has-text("count: 1")');
  await page.evaluate(() => (window.__hotProbe = "kept"));

  // A component edit: patched in place, state and page kept.
  fs.writeFileSync(widget, originals.get(widget).replace('"jsxglob:"', '"jsxhot:"'));
  await page.waitForSelector('[data-testid="glob-jsx"]:has-text("jsxhot:")', { timeout: 30000 });
  must((await page.textContent('[data-testid="counter"]')) === "count: 1", "the hot update reset component state");
  must((await page.evaluate(() => window.__hotProbe)) === "kept", "the hot update reloaded the page");
  // The server's own pass over the edit (its render reloads the module).
  must(
    await settles(() => log.includes("oj start: hot updated"), { timeoutMs: 30000, pollMs: 50 }),
    "the dev server did not report a hot update",
  );

  // A reload right after serves the edit from the server render and bundle.
  const html = await (await fetch(`http://localhost:${PORT}/`)).text();
  must(html.includes("jsxhot:"), "the server render after a hot update is stale");
  await page.reload();
  await page.waitForSelector('[data-testid="client-mounted"]', { timeout: 60000 });
  must(
    (await page.textContent('[data-testid="glob-jsx"]')).includes("jsxhot:"),
    "a reload after a hot update loaded the old bundle",
  );

  // An edit no module accepts (a plain function the route imports): the page
  // reloads into the fresh bundle.
  await page.evaluate(() => (window.__hotProbe = "kept"));
  const shouted = originals.get(format).replace("toUpperCase()", "toUpperCase() + '~'");
  must(shouted !== originals.get(format), "format.ts no longer has the edit point");
  fs.writeFileSync(format, shouted);
  await page.waitForFunction(() => window.__hotProbe === undefined, null, { timeout: 30000 });
  await page.waitForSelector('[data-testid="client-mounted"]', { timeout: 60000 });
  must(
    (await page.textContent("h1.fixture-heading")).includes("~"),
    "the reload after an unaccepted edit missed the edit",
  );
  must(errors.length === 0, `page errors: ${errors.join(" | ")}`);
  console.log("start hot update: component patched in place (state kept), reload fresh, unaccepted edit reloads");
} catch (e) {
  failed = true;
  console.error(e);
  console.error(log.split("\n").slice(-40).join("\n"));
} finally {
  await browser.close();
  srv.kill("SIGKILL");
  restore();
}
process.exit(failed ? 1 : 0);
