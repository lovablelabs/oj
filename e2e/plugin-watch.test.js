// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const { chromium } = require("playwright");
const fs = require("fs");
const path = require("path");

const FILE = path.join(__dirname, "..", "playground", "plugin-watched.txt");

(async () => {
  const original = fs.readFileSync(FILE, "utf8");
  const browser = await chromium.launch();
  const page = await browser.newPage();
  try {
    await page.goto("http://localhost:5199/", { waitUntil: "domcontentloaded" });
    await page.waitForSelector("main", { state: "attached", timeout: 15000 });
    await page.evaluate(() => (window.__watch_marker = 77));
    fs.writeFileSync(FILE, "watched-v2\n");
    await page.waitForFunction(() => window.__watch_marker === undefined, { timeout: 15000 });
    console.log("addWatchFile forced a full reload on a plain .txt change (marker dropped)");
    const marker = path.join(__dirname, "..", "playground", ".oj-cache", "plugin-watchchange");
    const wc = fs.existsSync(marker) ? fs.readFileSync(marker, "utf8").trim() : "MISSING";
    if (!wc.includes("plugin-watched.txt")) throw new Error("watchChange did not fire for the edit: " + wc);
    console.log("PLUGIN addWatchFile + watchChange HOOKS VERIFIED");
  } finally {
    fs.writeFileSync(FILE, original);
    await browser.close();
  }
})().catch((e) => {
  console.error("FAIL:", e.message);
  process.exit(1);
});
