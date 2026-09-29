// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const { chromium } = require("playwright");
const fs = require("fs");
const CSS = require("path").join(__dirname, "..", "playground") + "/styles.css";
(async () => {
  const original = fs.readFileSync(CSS, "utf8");
  const browser = await chromium.launch();
  const page = await browser.newPage();
  try {
    await page.goto("http://localhost:5199/", { waitUntil: "networkidle" });
    await page.waitForSelector("h1");
    await page.evaluate(() => {
      window.__marker = 1;
    });
    fs.writeFileSync(CSS, original + "\nbody { background: rgb(240, 240, 255); }\n");
    await page.waitForFunction(
      () => [...document.querySelectorAll("link[rel=stylesheet]")].some((l) => l.href.includes("?t=")),
      { timeout: 5000 },
    );
    await page.waitForFunction(() => getComputedStyle(document.body).backgroundColor === "rgb(240, 240, 255)", {
      timeout: 5000,
    });
    const marker = await page.evaluate(() => window.__marker);
    console.log("css applied without reload:", marker === 1 ? "yes" : "NO — page reloaded");
    if (marker !== 1) process.exit(1);
    console.log("CSS HOT SWAP VERIFIED");
  } finally {
    fs.writeFileSync(CSS, original);
    await browser.close();
  }
})().catch((e) => {
  console.error("FAIL:", e.message);
  process.exit(1);
});
