// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const { chromium } = require("playwright");
const fs = require("fs");
const path = require("path");

const APP = path.join(__dirname, "..", "playground") + "/src/App.tsx";
const URL = "http://localhost:5199/";
const DIALOG = 'div[role="dialog"][aria-label="Build error"]';

(async () => {
  const original = fs.readFileSync(APP, "utf8");
  const browser = await chromium.launch();
  const page = await browser.newPage();
  try {
    await page.goto(URL, { waitUntil: "networkidle" });
    await page.waitForSelector("h1:has-text('oj playground')", { timeout: 15000 });

    // Introduce a compile error; the dev server broadcasts a ws error → overlay.
    fs.writeFileSync(APP, original + "\nconst __oj_broken_syntax = ;\n");
    await page.waitForSelector(DIALOG, { timeout: 12000 });

    const brand = await page.locator(`${DIALOG} >> text=oj`).first().isVisible();
    const frame = (await page.locator(`${DIALOG} pre`).textContent()) || "";
    if (!brand) throw new Error("overlay missing the oj brand header");
    if (!frame.trim()) throw new Error("overlay has an empty code frame");
    console.log("overlay shown:      yes");
    console.log("frame non-empty:    yes");

    // Esc dismisses (backdrop/keyboard), not a click inside the card.
    await page.keyboard.press("Escape");
    await page.waitForSelector(DIALOG, { state: "detached", timeout: 5000 });
    console.log("esc dismissed:      yes");

    console.log("\nERROR OVERLAY VERIFIED: structured overlay + esc dismiss");
  } finally {
    fs.writeFileSync(APP, original);
    await browser.close();
  }
})().catch((err) => { console.error("FAIL:", err.message); process.exit(1); });
