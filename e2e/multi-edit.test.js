// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const { chromium } = require("playwright");
const fs = require("fs");
const path = require("path");

const APP = path.join(__dirname, "..", "playground", "src", "App.tsx");
(async () => {
  const original = fs.readFileSync(APP, "utf8");
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto("http://localhost:5199/", { waitUntil: "networkidle" });
    await page.waitForSelector("h1");
    await page.waitForTimeout(500);
    const btn = page.locator("button");
    await btn.click();
    await btn.click();

    fs.writeFileSync(APP, original.replace("oj playground", "oj playground EDIT1"));
    await page.waitForSelector("h1:has-text('EDIT1')", { timeout: 10000 });

    fs.writeFileSync(APP, original.replace("oj playground", "oj playground EDIT2"));
    await page.waitForSelector("h1:has-text('EDIT2')", { timeout: 10000 });

    const count = await btn.textContent();
    console.log("after 2 edits:", count.trim(), "| errors:", errors.length ? errors : "none");
    if (!count.includes("Clicks: 2")) throw new Error("STATE LOST across edits: " + count);
    if (errors.length) throw new Error("console errors");
    console.log("SEQUENTIAL PATCHES VERIFIED");
  } finally {
    fs.writeFileSync(APP, original);
    await browser.close();
  }
})().catch((e) => {
  console.error("FAIL:", e.message);
  process.exit(1);
});
