// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const { chromium } = require("playwright");
const fs = require("fs");
const path = require("path");

const FILE = path.join(__dirname, "..", "playground", "src", "hmr-demo.tsx");

(async () => {
  const original = fs.readFileSync(FILE, "utf8");
  const browser = await chromium.launch();
  const page = await browser.newPage();
  try {
    await page.goto("http://localhost:5199/", { waitUntil: "networkidle" });
    await page.waitForSelector("[data-hmr-demo]", { state: "attached" });
    await page.evaluate(() => (window.__hmr_marker = 123));
    fs.writeFileSync(FILE, original.replace('data-hmr-demo="v1"', 'data-hmr-demo="v2"'));
    await page.waitForFunction(() => window.__hmr_marker === undefined, { timeout: 10000 });
    await page.waitForSelector('[data-hmr-demo="v2"]', { state: "attached" });
    console.log("handleHotUpdate forced a full reload (marker dropped, v2 rendered)");
    console.log("PLUGIN handleHotUpdate HOOK VERIFIED");
  } finally {
    fs.writeFileSync(FILE, original);
    await browser.close();
  }
})().catch((e) => {
  console.error("FAIL:", e.message);
  process.exit(1);
});
