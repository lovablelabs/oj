// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

const { chromium } = require("playwright");
const fs = require("fs");
const APP = require("path").join(__dirname, "..", "playground") + "/src/App.tsx";
(async () => {
  const original = fs.readFileSync(APP, "utf8");
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  try {
    await page.goto("http://localhost:5199/", { waitUntil: "networkidle" });
    const h1 = page.locator("h1");
    await h1.waitFor();
    const deco = await h1.evaluate((el) => getComputedStyle(el).textDecorationLine);
    if (deco !== "underline") throw new Error("tailwind utility not applied: " + deco);
    const btn = page.locator("button");
    await btn.click();
    await btn.click();

    fs.writeFileSync(APP, original.replace('className="underline"', 'className="underline italic"'));
    await page.waitForFunction(() => getComputedStyle(document.querySelector("h1")).fontStyle === "italic", {
      timeout: 15000,
    });
    const text = await btn.textContent();
    console.log(
      "underline:",
      deco,
      "| italic after edit: yes | counter:",
      text.trim(),
      "| errors:",
      errors.length ? errors : "none",
    );
    if (!text.includes("Clicks: 2")) throw new Error("STATE LOST");
    if (errors.length) throw new Error("console errors");
    console.log("TAILWIND SIDECAR VERIFIED");
  } finally {
    fs.writeFileSync(APP, original);
    await browser.close();
  }
})().catch((e) => {
  console.error("FAIL:", e.message);
  process.exit(1);
});
